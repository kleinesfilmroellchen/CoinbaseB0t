//! Semantic analysis and AST datastructures.

use std::collections::HashMap;
use std::path::PathBuf;
use std::result::Result;
use std::sync::{Arc, Weak};

use miette::SourceSpan;
use parking_lot::RwLock;
#[allow(unused)]
use smartstring::alias::String;

use self::instruction::{AddressingMode, Instruction, Opcode};
use self::reference::{
	Label, MacroParameters, MacroParent, Reference, ReferenceResolvable, RelativeReferenceDirection,
};
use crate::assembler::resolve_file;
use crate::cli::{default_backend_options, Frontend};
use crate::directive::DirectiveValue;
use crate::error::AssemblyError;
use crate::parser::lexer::lex;
use crate::parser::{lalrpop_adaptor, Token};
use crate::sema::instruction::MemoryAddress;
use crate::{AssemblyCode, Directive, Segments};

pub mod instruction;
pub(crate) mod program;
pub(crate) mod reference;
pub(crate) mod register;
pub mod value;

pub use program::ProgramElement;
pub use register::Register;
pub use value::AssemblyTimeValue;

/// How a looked-up reference is used. See ``Environment::get_global_label``.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LabelUsageKind {
	/// Reference is used as a parameter, i.e. it's address is of interest.
	AsAddress,
	/// Label is being defined.
	AsDefinition,
}

/// Environment object for parsing. Holds the list of references.
#[derive(Debug)]
pub struct Environment {
	/// The list of global labels.
	pub globals: Vec<Arc<RwLock<Label>>>,
	/// The files included in this "tree" created by include statements.
	pub files:   HashMap<PathBuf, Arc<RwLock<AssemblyFile>>>,
	/// Error and warning options passed on the command line.
	pub options: Arc<dyn Frontend>,
}

impl Environment {
	/// Creates an empty environment.
	#[must_use]
	pub fn new() -> Arc<RwLock<Self>> {
		Arc::new(RwLock::new(Self { globals: Vec::new(), files: HashMap::new(), options: default_backend_options() }))
	}

	/// Sets the user-provided error options.
	pub fn set_error_options(&mut self, options: Arc<dyn Frontend>) {
		self.options = options;
	}

	/// Report or throw an error depending on what command-line options this assembly data object knows about. If error
	/// options are not available (on non-clap builds, e.g. tests), this always reports the error.
	/// # Errors
	/// The provided error is re-thrown if the error options specify to do so. On non-clap builds, this function never
	/// errors.
	#[allow(clippy::unnecessary_wraps, clippy::unused_self)]
	pub(crate) fn report_or_throw(&self, error: AssemblyError) -> Result<(), Box<AssemblyError>> {
		error.report_or_throw(&*self.options)
	}

	/// Searches for an existing parsed file in this environment given that file's source code.
	/// Note that the source code does not have to be the identical object in memory, it just has to compare equal.
	/// See ``AssemblyCode::eq`` for the equality semantics of the source code objects.
	pub(crate) fn find_file_by_source(
		&self,
		source_code: &Arc<AssemblyCode>,
	) -> Result<Option<Arc<RwLock<AssemblyFile>>>, Box<AssemblyError>> {
		self.files
			.get(&source_code.name)
			// Keep around a tuple with the original Arc so we can return it at the end.
			.map(|file|
				file.try_read_recursive().map(|borrowed_file| (file, borrowed_file)))
			.map(|maybe_file| {
				maybe_file.map(|(file, _)| file.clone())
				.ok_or_else(|| AssemblyError::IncludeCycle {
					cycle_trigger_file: source_code.file_name(),
					src:                source_code.clone(),
					include:            (0, 0).into(),
				}.into())
			}).transpose()
	}

	/// Parse a program given a set of tokens straight from the lexer.
	/// The parser makes sure that all pre-processing of the token stream and the final reference resolutions are
	/// performed.
	///
	/// In terms of multi-file behavior, the resulting file is added to this environment's list of source files. If the
	/// source code already was parsed in this environment, that parsed data is returned instead. If, however, the
	/// source code was parsed but didn't have its includes fully resolved yet, that constitutes an include cycle and an
	/// error is returned.
	///
	/// # Errors
	/// Whenever something goes wrong in parsing.
	pub fn parse(
		this: &Arc<RwLock<Self>>,
		tokens: Vec<Token>,
		source_code: &Arc<AssemblyCode>,
	) -> Result<Arc<RwLock<AssemblyFile>>, Box<AssemblyError>> {
		if let Some(already_parsed_file) = this.read_recursive().find_file_by_source(source_code)? {
			// If we're in a cycle, the already parsed file still has unresolved references.
			// I'm not sure whether this can happen in the first place given that find_file_by_source can't borrow such
			// a file and therefore won't return it, but let's better be safe than sorry.
			return if already_parsed_file.try_read_recursive().is_some_and(|file| file.has_unresolved_source_includes())
			{
				Err(AssemblyError::IncludeCycle {
					cycle_trigger_file: source_code.file_name(),
					src:                source_code.clone(),
					include:            (0, 0).into(),
				}
				.into())
			} else {
				Ok(already_parsed_file)
			};
		}

		let lexed = lalrpop_adaptor::preprocess_token_stream(tokens);
		let lalrpop_lexed = lalrpop_adaptor::LalrpopAdaptor::from(lexed.clone());
		let program = crate::parser::ProgramParser::new()
			.parse(this, source_code, lalrpop_lexed)
			.map_err(|err| AssemblyError::from_lalrpop(err, source_code.clone()))?;

		let rc_file = Arc::new(RwLock::new(AssemblyFile {
			content:     program,
			source_code: source_code.clone(),
			parent:      Arc::downgrade(this),
			tokens:      lexed,
		}));
		let mut file = rc_file.write();

		file.resolve_user_macro_arguments()?;
		file.coerce_to_direct_page_addressing();

		drop(file);
		// Insert the file into the list of source files so that we can detect cycles...
		this.write().files.insert(source_code.name.clone(), rc_file.clone());

		// ...once we start including source files here.
		let mut file = rc_file.write();
		file.resolve_source_includes()?;

		file.expand_user_macros()?;
		file.fill_in_reference_links()?;
		drop(file);

		Ok(rc_file)
	}

	/// Lookup a global label in this environment, and create it if necessary.
	pub fn get_global_label(
		&mut self,
		name: &'_ str,
		span: SourceSpan,
		usage_kind: LabelUsageKind,
	) -> Arc<RwLock<Label>> {
		if let Some(matching_reference) = self.globals.iter_mut().find(|reference| reference.read().name == name) {
			let mut mutable_matching_reference = matching_reference.write();
			if usage_kind == LabelUsageKind::AsAddress && !mutable_matching_reference.used_as_address {
				mutable_matching_reference.used_as_address = true;
			}
			// If the caller flags this use of the reference as its definition, we override the reference's position
			// with what we were just given.
			if usage_kind == LabelUsageKind::AsDefinition {
				mutable_matching_reference.span = span;
			}
			matching_reference.clone()
		} else {
			let new_reference = Label::new(name.into(), span);
			new_reference.write().used_as_address = usage_kind == LabelUsageKind::AsAddress;
			self.globals.push(new_reference.clone());
			new_reference
		}
	}
}

/// The AST and associated information for one file.
#[derive(Debug)]
pub struct AssemblyFile {
	/// Parsed contents.
	pub content:     Vec<ProgramElement>,
	/// Underlying source code and file name.
	pub source_code: Arc<AssemblyCode>,
	/// The environment that this file is parsed in.
	pub parent:      Weak<RwLock<Environment>>,
	/// The tokens that were parsed for this file.
	pub tokens:      Vec<Token>,
}

impl AssemblyFile {
	/// Returns the token at the given offset, if any.
	#[must_use]
	pub fn token_at(&self, offset: usize) -> Option<Token> {
		self.tokens.iter().find_map(|token| {
			let span = token.source_span();
			if span.offset() <= offset && span.offset() + span.len() > offset {
				Some(token.clone())
			} else {
				None
			}
		})
	}

	/// Returns the definitions of the given identifier string, if it is defined as a symbol anywhere.
	/// Note that things like local labels and relative references can be defined in multiple places.
	#[must_use]
	pub fn get_definition_spans_of(&self, identifier: &str) -> Vec<SourceSpan> {
		self.content
			.iter()
			.filter_map(|element| match element {
				ProgramElement::Label(label) if label.name() == identifier => Some(label.source_span()),
				ProgramElement::Directive(Directive {
					value: DirectiveValue::AssignReference { reference, .. },
					span,
					..
				}) if reference.name() == identifier => Some(*span),
				ProgramElement::Directive(Directive {
					value: DirectiveValue::UserDefinedMacro { name, .. },
					span,
					..
				}) if name == identifier => Some(*span),
				_ => None,
			})
			.collect()
	}

	/// Fills in the global label references for all local labels. Existing ones are overwritten, so the labels are
	/// always consistent.
	///
	/// # Errors
	/// If a local label precedes any global labels.
	/// # Panics
	/// All panics are programming errors.
	#[allow(clippy::too_many_lines)]
	pub fn fill_in_reference_links(&mut self) -> Result<(), Box<AssemblyError>> {
		let mut current_label: Option<Arc<RwLock<Label>>> = None;
		// While we do the local label merging, also perform the backward relative resolution.
		// The forward relative labels are resolved in a reverse pass afterwards.
		let mut current_backward_relative_label_map = HashMap::new();

		for element in &mut self.content {
			// First match for reference resolution in instruction position
			match element {
				ProgramElement::Label(ref mal @ Reference::MacroArgument { ref span, .. }) =>
					return Err(AssemblyError::UsingMacroArgumentOutsideMacro {
						name:     mal.to_string().into(),
						src:      self.source_code.clone(),
						location: *span,
					}
					.into()),
				ProgramElement::Label(Reference::Relative {
					direction: RelativeReferenceDirection::Backward,
					id,
					span,
					..
				}) => {
					// "Drop" the mutable borrow from id before *element is used to not confuse the borrow checker.
					let id = *id;
					// To reference the relative label until codegen, create a new local label for it.
					// This name is likely, but not guaranteed, to be unique! That's why we directly insert into
					// the globals list.
					let global_for_relative = Label::new(format!("ref_-_{}_{}", id, span.offset()).into(), *span);
					global_for_relative.write().used_as_address = true;
					self.parent
						.upgrade()
						.expect("parent disappeared")
						.write()
						.globals
						.push(global_for_relative.clone());
					*element = ProgramElement::Label(Reference::Label(global_for_relative.clone()));
					current_backward_relative_label_map.insert(id, global_for_relative);
				},

				ProgramElement::UserDefinedMacroCall { .. }
				| ProgramElement::IncludeSource { .. }
				| ProgramElement::Label(
					Reference::Label(_)
					| Reference::MacroGlobal { .. }
					| Reference::Relative { .. }
					| Reference::UnresolvedLocalLabel { .. },
				)
				| ProgramElement::Directive(_)
				| ProgramElement::Instruction(_) => (),
			}
			element.set_current_label(&current_label, &self.source_code)?;
			// If we just (newly) resolved this element to a label, set it as the current label.
			if let ProgramElement::Label(Reference::Label(ref label)) = element {
				current_label = Some(label.clone());
			}
			element.resolve_relative_labels(RelativeReferenceDirection::Backward, &current_backward_relative_label_map);
		}

		let mut current_forward_relative_label_map = HashMap::new();
		// Reverse iteration to resolve forward references.
		// Also use this second pass to resolve hierarchical local label shorthands (`global_local` for referencing
		// `global: .local: ...`)
		for element in self.content.iter_mut().rev() {
			if let ProgramElement::Label(Reference::Relative {
				direction: RelativeReferenceDirection::Forward,
				id,
				span,
				..
			}) = element
			{
				let id = *id;
				// To reference the relative label until codegen, create a new local label for it.
				let global_for_relative = Label::new(format!("ref_+_{}_{}", id, span.offset()).into(), *span);
				global_for_relative.write().used_as_address = true;
				self.parent.upgrade().expect("parent disappeared").write().globals.push(global_for_relative.clone());
				*element = ProgramElement::Label(Reference::Label(global_for_relative.clone()));
				current_forward_relative_label_map.insert(id, global_for_relative);
			}
			element.resolve_relative_labels(RelativeReferenceDirection::Forward, &current_forward_relative_label_map);
		}

		Ok(())
	}

	/// Fills in references to macro argument lists for macro argument occurrences.
	///
	/// # Errors
	/// If a macro argument with a wrong name was encountered.
	pub fn resolve_user_macro_arguments(&mut self) -> Result<(), Box<AssemblyError>> {
		for element in &mut self.content {
			if let ProgramElement::Directive(Directive {
				value: DirectiveValue::UserDefinedMacro { ref arguments, body, .. },
				..
			}) = element
			{
				for child_element in body {
					child_element.replace_macro_parent(arguments.clone(), &self.source_code)?;
				}
			}
		}
		Ok(())
	}

	/// Tries to coerce addressing modes to direct page addressing wherever possible. This needs to be done again as the
	/// unresolved local labels did not provide memory locations before merging.
	pub fn coerce_to_direct_page_addressing(&mut self) {
		for element in &mut self.content {
			if let ProgramElement::Instruction(Instruction {
				opcode: Opcode { first_operand, second_operand, force_direct_page, .. },
				..
			}) = element
			{
				let coercion_function = if *force_direct_page {
					AddressingMode::force_to_direct_page_addressing
				} else {
					AddressingMode::coerce_to_direct_page_addressing
				};
				*first_operand = first_operand.clone().map(AddressingMode::optimize_numbers).map(coercion_function);
				*second_operand = second_operand.clone().map(AddressingMode::optimize_numbers).map(coercion_function);
			}
		}
	}

	fn to_asm_error<'a>(
		span: &'a SourceSpan,
		source_code: &'a Arc<AssemblyCode>,
	) -> impl Fn(()) -> Box<AssemblyError> + 'a {
		|_| AssemblyError::MissingSegment { location: *span, src: source_code.clone() }.into()
	}

	/// Splits the AST into segments which (still) contain ``ProgramElements``. This is an initial step in laying out
	/// segments and their contents, and also helps to catch user segmenting errors early.
	///
	/// # Errors
	/// Any error that occurs during segment splitting.
	#[allow(clippy::missing_panics_doc)]
	pub fn split_into_segments(&self) -> Result<Segments<ProgramElement>, Box<AssemblyError>> {
		let mut segments = Segments::default();
		let mut brr_label_number = 0;
		let mut current_labels = Vec::default();
		for mut element in self.content.iter().cloned() {
			match element {
				ProgramElement::Label(reference) => {
					current_labels.push(reference.clone());
					segments
						.add_element(ProgramElement::Label(reference.clone()))
						.map_err(Self::to_asm_error(&reference.source_span(), &self.source_code))?;
					// Prevent the clearing of current labels.
					continue;
				},
				ProgramElement::Instruction(instruction) => segments
					.add_element(ProgramElement::Instruction(instruction.clone()))
					.map_err(Self::to_asm_error(&instruction.span, &self.source_code))?,
				ProgramElement::Directive(Directive { value: DirectiveValue::End, .. }) => break,
				ProgramElement::Directive(ref mut directive @ Directive { .. }) => {
					// All BRR directives that go into the BRR sample directory need a label, so we add a unique label
					// while we're here.
					if matches!(&directive.value, DirectiveValue::Brr { directory: true, .. })
						&& current_labels.is_empty()
					{
						let new_brr_label =
							Label::new(format!("brr_sample_{}", brr_label_number).into(), directive.span);
						new_brr_label.write().used_as_address = true;
						brr_label_number += 1;

						self.parent
							.upgrade()
							.ok_or_else(|| panic!("parent disappeared"))
							.unwrap()
							.write()
							.globals
							.push(new_brr_label.clone());
						segments
							.add_element(ProgramElement::Label(Reference::Label(new_brr_label.clone())))
							.map_err(Self::to_asm_error(&new_brr_label.read().span, &self.source_code))?;
						current_labels.push(Reference::Label(new_brr_label));
					}

					directive.perform_segment_operations_if_necessary(
						&mut segments,
						self.source_code.clone(),
						current_labels.first(),
					)?;
					if !directive.value.is_symbolic() {
						segments
							.add_element(ProgramElement::Directive(directive.clone()))
							.map_err(Self::to_asm_error(&directive.span, &self.source_code))?;
					}
				},
				ProgramElement::IncludeSource { .. } | ProgramElement::UserDefinedMacroCall { .. } => {},
			}
			current_labels.clear();
		}

		self.optimize_direct_page_labels(&mut segments);

		Ok(segments)
	}

	/// Optimizes long addressing instructions to use direct page addressing if the reference is in the direct page.
	/// This involves non-trivial semantic analysis:
	///
	/// 1. Collect all references, and all non-direct-page instructions referring to them; and their current
	/// locations.
	/// General hint: Calculate new reference positions by subtracting from every reference's position the number of
	/// dp-coerced instructions.
	/// 2. Assume all references lie in the direct page; i.e. the instructions can shrink by 1 byte.
	/// 3. Repeatedly find references (or reference-based calculations) that do not lie in the direct page anymore, and
	/// update instructions and positions. Repeat until no changes happen or the reference resolution limit is reached.
	/// 4. Modify instructions accordingly
	#[allow(clippy::too_many_lines)]
	fn optimize_direct_page_labels(&self, segments: &mut Segments<ProgramElement>) {
		/// Do the more complex direct page coercing with references. We only consider references in the direct page if
		/// they are in the *zero page*. If that is a problem, forcing to direct page addressing is always possible.
		#[derive(Debug, Clone)]
		enum InstructionOrReference {
			Instruction {
				/// The index is required to recover the actual instruction and modify it later on. It is purely
				/// symbolic and has nothing to do with the instruction's address.
				index_in_segment: usize,
				// Each reference also stores the exact calculation that it is used in. Even if the reference itself is
				// in the direct page, the calculated location may not be!
				references:       Vec<(Reference, AssemblyTimeValue)>,
				// Whether this instruction is (now) short, i.e. in direct page mode.
				is_short:         bool,
			},
			Reference(Reference),
		}

		#[derive(Debug, Clone)]
		struct ReferencedObject {
			// Currently expected address of this referenced object.
			address:       MemoryAddress,
			/// Start address of the segment this referenced object is within.
			segment_start: MemoryAddress,
			object:        InstructionOrReference,
		}

		let max_coercion_passes =
			self.parent.upgrade().expect("parent disappeared").read().options.maximum_reference_resolution_passes();

		// 1. (collect relevant objects)
		let mut referenced_objects = Vec::<ReferencedObject>::new();

		for (segment_start, segment_contents) in &segments.segments {
			for (index_in_segment, (element, offset)) in segment_contents
				.iter()
				.scan(0.into(), |offset: &mut MemoryAddress, element| {
					let return_value = Some((element, *offset));
					*offset += element.assembled_size() as MemoryAddress;
					return_value
				})
				.enumerate()
			{
				match element {
					ProgramElement::Label(label) => referenced_objects.push(ReferencedObject {
						address:       offset + *segment_start,
						segment_start: *segment_start,
						object:        InstructionOrReference::Reference(label.clone()),
					}),
					ProgramElement::Instruction(Instruction { opcode, .. }) => {
						let references = opcode.references_and_calculations();
						if opcode.has_long_address()
							&& !references.is_empty() && opcode.can_use_direct_page_addressing()
						{
							referenced_objects.push(ReferencedObject {
								address:       offset + *segment_start,
								segment_start: *segment_start,
								object:        InstructionOrReference::Instruction {
									index_in_segment,
									references: references
										.into_iter()
										.map(|(reference, value)| (reference.clone(), value.clone()))
										.collect(),
									is_short: false,
								},
							});
						}
					},
					ProgramElement::Directive(Directive { .. }) => (),
					ProgramElement::IncludeSource { .. } | ProgramElement::UserDefinedMacroCall { .. } => panic!(
						"source includes and unresolved macro calls at reference optimization time, this is a bug"
					),
				}
			}
		}

		// 2. (assume direct page references everywhere)
		// Store by how much later objects need to be offset forwards.
		let mut address_offset = 0;
		let mut last_segment = None;
		for ReferencedObject { address, segment_start, object } in &mut referenced_objects {
			// New segment started, let's reset the address offset since segments don't influence each other.
			if let Some(last_segment) = last_segment && *segment_start != last_segment {
				address_offset = 0;
			}
			*address += address_offset;
			if let InstructionOrReference::Instruction { is_short, .. } = object {
				address_offset -= 1;
				*is_short = true;
			}
			last_segment = Some(*segment_start);
		}

		let mut iteration = 1;
		let mut change = true;
		// 3.
		while change && iteration <= max_coercion_passes {
			change = false;

			let all_references = referenced_objects
				.iter()
				.filter(|ReferencedObject { object: candidate, .. }| {
					matches!(candidate, InstructionOrReference::Reference(_))
				})
				.cloned()
				.collect::<Vec<_>>();
			let find_value_for_reference = |queried_reference| {
				for ReferencedObject { address, object, .. } in &all_references {
					if let InstructionOrReference::Reference(candidate) = object && candidate == &queried_reference {
						return Some(*address);
					}
				}
				None
			};

			let mut address_offset = 0;
			let mut last_segment = None;
			for ReferencedObject { address, object, segment_start } in &mut referenced_objects {
				if let Some(last_segment) = last_segment && *segment_start != last_segment {
					address_offset = 0;
				}
				*address += address_offset;
				match object {
					InstructionOrReference::Instruction { references, is_short, .. }
						if references.iter().any(|(_, value)| {
							// Usefully, if the resolver can't help with some of the references, we just fall back to
							// long addressing mode automatically.
							matches!(value.value_using_resolver(&find_value_for_reference), Some(0x100 ..) | None)
						}) =>
					{
						change = true;
						*is_short = false;
						address_offset += 1;
					},
					_ => {},
				}
				last_segment = Some(*segment_start);
			}

			iteration += 1;
		}

		// 4.
		for (segment_start, index_in_segment) in
			referenced_objects.iter().filter_map(|ReferencedObject { segment_start, object, .. }| match object {
				InstructionOrReference::Instruction { index_in_segment, is_short: true, .. } =>
					Some((*segment_start, *index_in_segment)),
				_ => None,
			}) {
			let instruction_segment = segments.segments.get_mut(&segment_start).unwrap();
			if let ProgramElement::Instruction(instruction) = &mut instruction_segment[index_in_segment] {
				if let Some(op) = instruction.opcode.first_operand.as_mut() {
					*op = op.clone().force_to_direct_page_addressing();
				}
				if let Some(op) = instruction.opcode.second_operand.as_mut() {
					*op = op.clone().force_to_direct_page_addressing();
				}
			}
		}
	}

	/// Returns whether this file's parsed content contains any unresolved include directives.
	pub(crate) fn has_unresolved_source_includes(&self) -> bool {
		self.content.iter().any(|element| matches!(element, ProgramElement::IncludeSource { .. }))
	}

	/// Resolves all source include directives by recursively calling into lexer and parser.
	///
	/// # Errors
	/// All errors from other files are propagated, as well as include cycles.
	pub fn resolve_source_includes(&mut self) -> Result<(), Box<AssemblyError>> {
		let mut index = 0;
		while index < self.content.len() {
			let element = self.content[index].clone();
			if let ProgramElement::IncludeSource { ref file, span } = element {
				let environment = self.parent.upgrade().expect("parent deleted while we're still parsing");
				let file: String = resolve_file(&self.source_code, file).to_string_lossy().into();
				let mut included_code =
					AssemblyCode::from_file(&file).map_err(|os_error| AssemblyError::FileNotFound {
						os_error:  Arc::new(os_error),
						file_name: file,
						src:       self.source_code.clone(),
						location:  span,
					})?;
				let child_include_path = &mut unsafe { Arc::get_mut_unchecked(&mut included_code) }.include_path;
				child_include_path.push(self.source_code.name.clone());
				child_include_path.append(&mut self.source_code.include_path.clone());

				let tokens = lex(included_code.clone(), &*environment.read().options)?;
				let included_file = Environment::parse(&environment, tokens, &included_code)?;

				self.content.splice(index ..= index, included_file.read().content.clone());
				continue;
			}
			index += 1;
		}
		Ok(())
	}

	/// Expands calls to user-defined macros.
	///
	/// # Errors
	/// Any errors relating to macro calls and macro definitions.
	pub fn expand_user_macros(&mut self) -> Result<(), Box<AssemblyError>> {
		let maximum_macro_expansion_depth = self
			.parent
			.upgrade()
			.expect("environment destroyed before assembly file")
			.read()
			.options
			.maximum_macro_expansion_depth();

		let user_macros = self
			.content
			.iter()
			.filter_map(|el| match el {
				ProgramElement::Directive(Directive {
					span,
					value: value @ DirectiveValue::UserDefinedMacro { name, .. },
					..
				}) => Some((name.clone(), (*span, value.clone()))),
				_ => None,
			})
			.collect::<HashMap<_, _>>();

		let mut index = 0;
		// A stack of end indices where code inserted by macros ends. Specifically, the indices point at the first
		// program element after the macro. This is used to keep track of recursion depth.
		let mut macro_end_stack = Vec::new();

		while index < self.content.len() {
			let element = &mut self.content[index];

			if let ProgramElement::UserDefinedMacroCall { macro_name, arguments: actual_arguments, span, .. } = element
			{
				if macro_end_stack.len() > maximum_macro_expansion_depth {
					return Err(AssemblyError::RecursiveMacroUse {
						depth:    maximum_macro_expansion_depth,
						name:     macro_name.clone(),
						location: *span,
						src:      self.source_code.clone(),
					}
					.into());
				}

				let called_macro = user_macros.get(macro_name);
				if let Some((definition_span, DirectiveValue::UserDefinedMacro { arguments, body, .. })) = called_macro
				{
					let arguments = arguments.read();
					let formal_arguments = match &(arguments).parameters {
						MacroParameters::Formal(formal_arguments) => formal_arguments,
						MacroParameters::Actual(_) => unreachable!(),
					};
					if formal_arguments.len() != actual_arguments.len() {
						return Err(AssemblyError::IncorrectNumberOfMacroArguments {
							name:            macro_name.clone(),
							expected_number: formal_arguments.len(),
							actual_number:   actual_arguments.len(),
							location:        *span,
							definition:      *definition_span,
							src:             self.source_code.clone(),
						}
						.into());
					}
					let actual_argument_parent = MacroParent::new_actual(
						formal_arguments
							.iter()
							.zip(actual_arguments.iter())
							.map(|((formal_argument, _), actual_argument)| {
								(formal_argument.clone(), actual_argument.clone())
							})
							.collect(),
						// We use a unique reference name just to make sure that we don't combine different
						// references accidentally.
						Label::new(format!("{}_global_label_{}", macro_name, index).into(), *definition_span),
					);
					// FIXME: Doesn't handle macro-internal references correctly; also no support for the \@ special
					// label.
					let mut inserted_body = body.clone();
					for macro_element in &mut inserted_body {
						macro_element.replace_macro_parent(actual_argument_parent.clone(), &self.source_code)?;
					}

					let body_length = inserted_body.len();
					self.content.splice(index ..= index, inserted_body);

					// Shift all later end indices backwards to account for the inserted instructions.
					macro_end_stack = macro_end_stack
						.into_iter()
						.map(|end_index| if end_index >= index { end_index + body_length } else { end_index })
						.collect();
					macro_end_stack.push(index + body_length);
					continue;
				}
				return Err(AssemblyError::UndefinedUserMacro {
					name:             macro_name.clone(),
					available_macros: user_macros.keys().map(String::clone).collect(),
					location:         *span,
					src:              self.source_code.clone(),
				}
				.into());
			}
			index += 1;
			// Using drain_filter is the easiest way of filtering elements from a vector. We need to consume the
			// returned iterator fully or else not all filtering will happen.
			let _: usize = macro_end_stack.drain_filter(|end_index| *end_index < index).count();
		}

		Ok(())
	}
}
