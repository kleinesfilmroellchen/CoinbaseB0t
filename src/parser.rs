//! Parsing and AST.
use std::collections::HashMap;
use std::result::Result;
use std::sync::Arc;

use miette::{SourceOffset, SourceSpan};

use super::error::{AssemblyCode, AssemblyError};
use super::instruction::{AddressingMode, Instruction, Mnemonic, Number, Opcode};
use super::label::{GlobalLabel, Label, LocalLabel};
use super::{ProgramElement, Register, Token};
use crate::error::TokenOrString;
use crate::token::TokenStream;
use crate::Macro;

/// Anything that can be primitively parsed from a string into an enum variant.
/// This trait is intended to be derived with the macro from ``spcasm_derive``.
pub trait Parse
where
	Self: Sized,
{
	/// Parse this enum from the string representation.
	/// # Errors
	/// If the string doesn't correspond with any enum variant.
	fn parse(value: &str, location: SourceSpan, src: Arc<AssemblyCode>) -> Result<Self, AssemblyError>;

	/// Returns whether this string corresponds with an enum variant; i.e. parsing would succeed.
	fn is_valid(value: &str) -> bool;
}

/// Environment object for parsing. Holds the list of labels.
#[derive(Debug)]
pub struct Environment {
	/// The list of labels.
	pub labels:      Vec<Arc<GlobalLabel>>,
	/// The source code of the assembly code.
	pub source_code: Arc<AssemblyCode>,
}

impl Environment {
	/// Creates an empty environment.
	#[must_use]
	pub const fn new(source_code: Arc<AssemblyCode>) -> Self {
		Self { labels: Vec::new(), source_code }
	}

	/// Parses the token stream into a list of instructions while keeping track of labels internally. Note that no label
	/// resolution is actually done.
	///
	/// # Errors
	/// Any parser error is returned as a string.
	/// # Panics
	/// All the panics are programming bugs.
	pub fn parse(&mut self, tokens: &[Token]) -> Result<Vec<ProgramElement>, AssemblyError> {
		let mut tokens = TokenStream::new(tokens, &self.source_code);
		let mut instructions = Vec::new();
		let mut current_global_label = None;
		let mut label_for_next_instruction = None;

		while let Ok(token) = tokens.next() {
			match &token {
				Token::Identifier(identifier, location) => {
					let location_span = SourceOffset::from(location.offset());
					// Global label
					current_global_label = Some(self.get_global_label(identifier, token.source_span(), false));
					label_for_next_instruction = Some(Label::Global(current_global_label.clone().unwrap()));
					tokens.expect(&Token::Colon(location_span))?;
				},
				Token::Mnemonic(mnemonic, location) => {
					let location_span = SourceOffset::from(location.offset());
					let newline = Token::Newline(location_span);
					// Instruction
					let mut tokens_for_instruction = tokens.make_substream();
					tokens_for_instruction.limit_to_first(&newline);
					tokens.advance_to_others_end(&tokens_for_instruction)?;

					instructions.push(ProgramElement::Instruction(self.create_instruction(
						*mnemonic,
						token.source_span(),
						tokens_for_instruction,
						label_for_next_instruction,
						current_global_label.clone(),
					)?));
					label_for_next_instruction = None;
					if !tokens.is_end() {
						tokens.expect(&newline)?;
					}
				},
				Token::Macro(symbol, location) => {
					// Macro
					let newline = Token::Newline(location.offset().into());
					let mut tokens_for_macro = tokens.make_substream();
					tokens_for_macro.limit_to_first(&newline);
					tokens.advance_to_others_end(&tokens_for_macro)?;

					instructions.push(ProgramElement::Macro(Macro::parse_macro(
						self,
						*symbol,
						*location,
						tokens_for_macro,
						label_for_next_instruction,
						current_global_label.clone(),
					)?));
					label_for_next_instruction = None;
					if !tokens.is_end() {
						tokens.expect(&newline)?;
					}
				},
				Token::Newline(..) => {},
				Token::Period(location) => {
					// Local label
					let expected_identifier = Token::Identifier("label".to_owned(), (*location).into());
					let (label_name, label_location) = match tokens.expect(&expected_identifier)? {
						Token::Identifier(name, location) => (name.clone(), location),
						_ => unreachable!(),
					};
					tokens.expect(&Token::Colon(*location))?;
					let local_label = Label::Local(LocalLabel::new(
						label_name.clone(),
						SourceSpan::new(
							*location,
							SourceOffset::from((label_location.offset() - location.offset()) + label_location.len()),
						),
						&current_global_label.clone().ok_or_else(|| AssemblyError::MissingGlobalLabel {
							local_label: label_name,
							src:         self.source_code.clone(),
							location:    label_location,
						})?,
					));
					label_for_next_instruction = Some(local_label);
				},
				actual =>
					return Err(AssemblyError::ExpectedToken {
						expected: Token::Identifier("identifier".to_owned(), token.source_span()),
						actual:   actual.clone(),
						location: token.source_span(),
						src:      self.source_code.clone(),
					}),
			}
		}

		Ok(instructions)
	}

	fn create_instruction(
		&mut self,
		mnemonic: Mnemonic,
		location: SourceSpan,
		mut tokens: TokenStream<'_>,
		label: Option<Label>,
		current_global_label: Option<Arc<GlobalLabel>>,
	) -> Result<Instruction, AssemblyError> {
		match mnemonic {
			Mnemonic::Mov
			| Mnemonic::Adc
			| Mnemonic::Sbc
			| Mnemonic::And
			| Mnemonic::Or
			| Mnemonic::Eor
			| Mnemonic::Cmp
			| Mnemonic::Movw
			| Mnemonic::Addw
			| Mnemonic::Subw
			| Mnemonic::Cmpw
			| Mnemonic::Div
			| Mnemonic::Bbs
			| Mnemonic::Bbc
			| Mnemonic::Cbne
			| Mnemonic::Dbnz
			| Mnemonic::And1
			| Mnemonic::Or1
			| Mnemonic::Eor1
			| Mnemonic::Mov1 => self.make_two_operand_instruction(mnemonic, &mut tokens, label, location, current_global_label),
			Mnemonic::Inc
			| Mnemonic::Dec
			| Mnemonic::Asl
			| Mnemonic::Lsr
			| Mnemonic::Rol
			| Mnemonic::Ror
			| Mnemonic::Incw
			| Mnemonic::Decw
			| Mnemonic::Daa
			| Mnemonic::Das
			| Mnemonic::Mul
			| Mnemonic::Bra
			| Mnemonic::Beq
			| Mnemonic::Bne
			| Mnemonic::Bcs
			| Mnemonic::Bcc
			| Mnemonic::Bvs
			| Mnemonic::Bvc
			| Mnemonic::Bmi
			| Mnemonic::Bpl
			| Mnemonic::Jmp
			| Mnemonic::Call
			| Mnemonic::Pcall
			| Mnemonic::Tcall
			| Mnemonic::Push
			| Mnemonic::Xcn
			| Mnemonic::Pop
			| Mnemonic::Set1
			| Mnemonic::Clr1
			| Mnemonic::Tset1
			| Mnemonic::Tclr1
			| Mnemonic::Not1 =>
				self.make_single_operand_instruction(mnemonic, &mut tokens, label, location, current_global_label),
			Mnemonic::Brk
			| Mnemonic::Ret
			| Mnemonic::Ret1
			| Mnemonic::Clrc
			| Mnemonic::Setc
			| Mnemonic::Notc
			| Mnemonic::Clrv
			| Mnemonic::Clrp
			| Mnemonic::Setp
			| Mnemonic::Ei
			| Mnemonic::Di
			| Mnemonic::Nop
			| Mnemonic::Sleep
			| Mnemonic::Stop => self.make_zero_operand_instruction(mnemonic, &mut tokens, label, location),
		}
	}

	fn make_two_operand_instruction(
		&mut self,
		mnemonic: Mnemonic,
		tokens: &mut TokenStream<'_>,
		label: Option<Label>,
		mnemonic_token_location: SourceSpan,
		current_global_label: Option<Arc<GlobalLabel>>,
	) -> Result<Instruction, AssemblyError> {
		let mut first_addressing_mode = tokens.make_substream();
		first_addressing_mode.limit_to_first(&Token::Comma(SourceOffset::from(0)));
		let mut second_addressing_mode = tokens.make_substream();
		second_addressing_mode.advance_to_others_end(&first_addressing_mode)?;
		// We need to also advance past the comma.
		second_addressing_mode.next()?;

		if first_addressing_mode.is_end() {
			return Err(AssemblyError::UnexpectedEndOfTokens {
				expected: "first argument".into(),
				location: mnemonic_token_location,
				src:      self.source_code.clone(),
			});
		}
		if second_addressing_mode.is_end() {
			return Err(AssemblyError::UnexpectedEndOfTokens {
				expected: "second argument".into(),
				location: first_addressing_mode.end().unwrap().source_span(),
				src:      self.source_code.clone(),
			});
		}

		let first_addressing_mode =
			self.parse_addressing_mode(&mut first_addressing_mode, current_global_label.clone())?;
		let second_addressing_mode = self.parse_addressing_mode(&mut second_addressing_mode, current_global_label)?;

		#[cfg(test)]
		let expected_value = tokens.iter().find_map(|token| match token {
			Token::TestComment(expected_value, ..) => Some(expected_value.clone()),
			_ => None,
		});
		let final_span = tokens.end().unwrap().source_span();
		let instruction = Instruction {
			opcode: Opcode::make_two_operand_instruction(mnemonic, first_addressing_mode, second_addressing_mode),
			label,
			span: (
				mnemonic_token_location.offset(),
				(final_span.offset() + final_span.len()) - mnemonic_token_location.offset(),
			)
				.into(),
			#[cfg(test)]
			expected_value,
			#[cfg(test)]
			assembled_size: None,
		};
		Ok(instruction)
	}

	fn make_single_operand_instruction(
		&mut self,
		mnemonic: Mnemonic,
		tokens: &mut TokenStream<'_>,
		label: Option<Label>,
		mnemonic_token_location: SourceSpan,
		current_global_label: Option<Arc<GlobalLabel>>,
	) -> Result<Instruction, AssemblyError> {
		let addressing_mode = self.parse_addressing_mode(tokens, current_global_label)?;
		#[cfg(test)]
		let expected_value = tokens.iter().find_map(|token| match token {
			Token::TestComment(expected_value, ..) => Some(expected_value.clone()),
			_ => None,
		});
		let final_span = tokens.end().unwrap().source_span();
		let instruction = Instruction {
			opcode: Opcode::make_single_operand_instruction(mnemonic, addressing_mode),
			label,
			span: (
				mnemonic_token_location.offset(),
				(final_span.offset() + final_span.len()) - mnemonic_token_location.offset(),
			)
				.into(),
			#[cfg(test)]
			expected_value,
			#[cfg(test)]
			assembled_size: None,
		};
		Ok(instruction)
	}

	fn make_zero_operand_instruction(
		&self,
		mnemonic: Mnemonic,
		tokens: &mut TokenStream<'_>,
		label: Option<Label>,
		mnemonic_token_location: SourceSpan,
	) -> Result<Instruction, AssemblyError> {
		if tokens
			.iter()
			.filter(|token| match token {
				Token::Newline(..) => false,
				#[cfg(test)]
				Token::TestComment(..) => false,
				_ => true,
			})
			.count() == 0
		{
			#[cfg(test)]
			let expected_value = tokens.iter().find_map(|token| match token {
				Token::TestComment(expected_value, ..) => Some(expected_value.clone()),
				_ => None,
			});
			let instruction = Instruction {
				opcode: Opcode { mnemonic, first_operand: None, second_operand: None },
				label,
				span: mnemonic_token_location,
				#[cfg(test)]
				expected_value,
				#[cfg(test)]
				assembled_size: None,
			};
			Ok(instruction)
		} else {
			Err(AssemblyError::OperandNotAllowed {
				mnemonic,
				location: mnemonic_token_location,
				src: self.source_code.clone(),
			})
		}
	}

	#[allow(clippy::too_many_lines)]
	fn parse_addressing_mode(
		&mut self,
		tokens: &mut TokenStream<'_>,
		current_global_label: Option<Arc<GlobalLabel>>,
	) -> Result<AddressingMode, AssemblyError> {
		let source_code_copy = self.source_code.clone();
		let end_location = tokens.end().map_or((0, 0).into(), Token::source_span);
		let missing_token_error = |expected| {
			|| AssemblyError::UnexpectedEndOfTokens { expected, location: end_location, src: source_code_copy.clone() }
		};

		match tokens.next()? {
			Token::Register(name, ..) => Ok(AddressingMode::Register(name)),
			Token::Hash(..) => Ok(AddressingMode::Immediate(self.parse_number(tokens, current_global_label)?)),
			// Direct address modes
			Token::Number(..) | Token::Identifier(..) | Token::Period(..) | Token::Plus(..) => {
				tokens.backtrack(1);
				let literal = self.parse_number(tokens, current_global_label)?.try_resolve();
				let is_direct_page = match literal {
					Number::Literal(address) => address <= 0xFF,
					_ => false,
				};
				let next_token_or_none = tokens.next().ok();
				Ok(match next_token_or_none {
					// Indirect addressing with '+X' or '+Y'
					Some(Token::Plus(..)) => match tokens.next()? {
						Token::Register(Register::X, ..) =>
							if is_direct_page {
								AddressingMode::DirectPageXIndexed(literal)
							} else {
								AddressingMode::XIndexed(literal)
							},
						Token::Register(Register::Y, ..) =>
							if is_direct_page {
								AddressingMode::DirectPageYIndexed(literal)
							} else {
								AddressingMode::YIndexed(literal)
							},
						reg =>
							return Err(AssemblyError::InvalidIndexingToken {
								token:    reg.clone(),
								location: reg.source_span(),
								src:      self.source_code.clone(),
							}),
					},
					// Bit indexing mode
					#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
					Some(Token::Period(token_start)) =>
						if let Ok(Token::Number(bit, location)) = tokens.next() {
							if !(0 .. 8).contains(&bit) {
								return Err(AssemblyError::InvalidBitIndex {
									index: bit as u8,
									location,
									src: self.source_code.clone(),
								});
							}
							if is_direct_page {
								AddressingMode::DirectPageBit(literal, (bit & 0x07) as u8)
							} else {
								AddressingMode::AddressBit(literal, (bit & 0x07) as u8)
							}
						} else {
							return Err(missing_token_error(Token::Number(0, token_start.into()).into())());
						},
					None | Some(Token::Newline(..)) =>
						if is_direct_page {
							AddressingMode::DirectPage(literal)
						} else {
							AddressingMode::Address(literal)
						},
					#[cfg(test)]
					Some(Token::TestComment(..)) =>
						if is_direct_page {
							AddressingMode::DirectPage(literal)
						} else {
							AddressingMode::Address(literal)
						},
					Some(other_token) =>
						return Err(AssemblyError::ExpectedToken {
							expected: Token::Plus(other_token.source_span().offset().into()),
							actual:   other_token.clone(),
							location: other_token.source_span(),
							src:      self.source_code.clone(),
						}),
				})
			},
			// Negated bit index
			#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
			Token::Slash(location) => {
				let number = self.parse_number(tokens, current_global_label)?;
				tokens.expect(&Token::Period(location))?;
				let (bit, end_location) = match tokens.expect(&Token::Number(0, location.into()))? {
					Token::Number(number, location) => (number, location),
					_ => unreachable!(),
				};
				if !(0x00 ..= 0x07).contains(&bit) {
					return Err(AssemblyError::InvalidBitIndex {
						index:    bit as u8,
						location: end_location,
						src:      self.source_code.clone(),
					});
				}

				Ok(AddressingMode::NegatedAddressBit(number, bit as u8))
			},
			// Indexed modes
			Token::OpenParenthesis(location) => match tokens.next()? {
				// (X), (Y), ...
				ref register_token @ Token::Register(ref name, ..) => {
					tokens.expect(&Token::CloseParenthesis(location))?;
					Ok(match name {
						#[allow(clippy::branches_sharing_code)]
						Register::X => {
							if tokens.expect(&Token::Plus(location)).is_ok() {
								if let Ok(further_token) = tokens.next() {
									println!(
										"{:?}",
										miette::Report::new(AssemblyError::DanglingTokens {
											src:      self.source_code.clone(),
											location: further_token.source_span(),
										})
									);
								}
								// '+' after closing bracket
								AddressingMode::IndirectXAutoIncrement
							} else {
								if let Ok(further_token) = tokens.next() {
									println!(
										"{:?}",
										miette::Report::new(AssemblyError::DanglingTokens {
											src:      self.source_code.clone(),
											location: further_token.source_span(),
										})
									);
								}
								AddressingMode::IndirectX
							}
						},
						Register::Y => {
							if let Ok(further_token) = tokens.next() {
								println!(
									"{:?}",
									miette::Report::new(AssemblyError::DanglingTokens {
										src:      self.source_code.clone(),
										location: further_token.source_span(),
									})
								);
							}
							AddressingMode::IndirectY
						},
						_ =>
							return Err(AssemblyError::InvalidIndexingToken {
								token:    register_token.clone(),
								location: register_token.source_span(),
								src:      self.source_code.clone(),
							}),
					})
				},
				// (address) ...
				literal_token @ (Token::Number(..) | Token::Identifier(..)) => {
					let literal = self.create_literal(&literal_token, true)?;
					match tokens.next()? {
						Token::Plus(second_location) => {
							tokens.expect(&Token::Register(Register::X, (location, second_location).into()))?;
							tokens.expect(&Token::CloseParenthesis(location))?;
							if let Ok(further_token) = tokens.next() {
								println!(
									"{:?}",
									miette::Report::new(AssemblyError::DanglingTokens {
										src:      self.source_code.clone(),
										location: further_token.source_span(),
									})
								);
							}
							Ok(AddressingMode::DirectPageXIndexedIndirect(literal))
						},
						Token::CloseParenthesis(second_location) => {
							let span = (location, second_location).into();
							tokens.expect(&Token::Plus(location))?;
							let result = tokens
								.expect(&Token::Register(Register::Y, span))
								.map(|_| AddressingMode::DirectPageIndirectYIndexed(literal));
							if let Ok(further_token) = tokens.next() {
								println!(
									"{:?}",
									miette::Report::new(AssemblyError::DanglingTokens {
										src:      self.source_code.clone(),
										location: further_token.source_span(),
									})
								);
							}
							result
						},
						wrong_token => Err(AssemblyError::ExpectedToken {
							expected: Token::CloseParenthesis(wrong_token.source_span().offset().into()),
							actual:   wrong_token.clone(),
							location: wrong_token.source_span(),
							src:      self.source_code.clone(),
						}),
					}
				},
				wrong_token => Err(AssemblyError::ExpectedToken {
					expected: Token::Number(0, wrong_token.source_span().offset().into()),
					actual:   wrong_token.clone(),
					location: wrong_token.source_span(),
					src:      self.source_code.clone(),
				}),
			},
			wrong_token => Err(AssemblyError::ExpectedToken {
				expected: Token::Number(0, wrong_token.source_span().offset().into()),
				actual:   wrong_token.clone(),
				location: wrong_token.source_span(),
				src:      self.source_code.clone(),
			}),
		}
	}

	/// Lookup a global label in this environment, and create it if necessary.
	pub fn get_global_label(&mut self, name: &'_ str, span: SourceSpan, used_as_address: bool) -> Arc<GlobalLabel> {
		if let Some(matching_label) = self.labels.iter_mut().find(|label| label.name == name) {
			if used_as_address && !matching_label.used_as_address {
				unsafe { Arc::get_mut_unchecked(matching_label).used_as_address = true };
			}
			matching_label.clone()
		} else {
			let new_label = Arc::new(GlobalLabel {
				name: name.to_owned(),
				location: None,
				span,
				used_as_address,
				locals: HashMap::new(),
			});
			self.labels.push(new_label.clone());
			new_label
		}
	}

	// Parse a number; which can be a statically resolvable expression.
	pub(crate) fn parse_number(
		&mut self,
		tokens: &mut TokenStream,
		current_global_label: Option<Arc<GlobalLabel>>,
	) -> Result<Number, AssemblyError> {
		let lhs = match tokens.next()? {
			literal @ (Token::Number(..) | Token::Identifier(..)) => self.create_literal(&literal, false),
			Token::Period(.., span) => {
				// Local label
				let (local_label, label_span) =
					match tokens.expect(&Token::Identifier("local label".to_string(), span.into()))? {
						Token::Identifier(name, label_span) => (name, label_span),
						_ => unreachable!(),
					};
				Ok(Number::Label(Label::Local(LocalLabel {
					span:     label_span,
					name:     local_label.clone(),
					parent:   Arc::downgrade(&current_global_label.clone().ok_or(
						AssemblyError::MissingGlobalLabel {
							local_label,
							src: self.source_code.clone(),
							location: label_span,
						},
					)?),
					location: None,
				})))
			},
			Token::OpenParenthesis(span) => {
				// Parse a sub expression with a recursive call. We'll pass on the same token stream so that everything
				// up to the ) is consumed.
				let result = self.parse_number(tokens, current_global_label.clone())?;
				tokens.expect(&Token::CloseParenthesis(span))?;
				Ok(result)
			},
			// '+' does of course not require a closing parenthesis unlike above.
			Token::Plus(..) => self.parse_number(tokens, current_global_label.clone()),
			Token::Newline(span) => Err(AssemblyError::UnexpectedEndOfTokens {
				expected: TokenOrString::Token(Token::Number(0, span.into())),
				location: span.into(),
				src:      self.source_code.clone(),
			}),
			token => Err(AssemblyError::ExpectedToken {
				expected: Token::Number(0, token.source_span()),
				actual:   token.clone(),
				location: token.source_span(),
				src:      self.source_code.clone(),
			}),
		}?;

		// It's totally fine if we hit various tokens not part of the expression anymore, or we are at the end of our
		// stream. Just return the lhs.
		match tokens.next() {
			Err(_) => Ok(lhs),
			// All of these must remain available for the caller.
			Ok(Token::Newline(..) | Token::Period(..) | Token::CloseParenthesis(..) | Token::Comma(..)) => {
				tokens.backtrack(1);
				Ok(lhs)
			},
			#[cfg(test)]
			Ok(Token::TestComment(..)) => {
				tokens.backtrack(1);
				Ok(lhs)
			},
			Ok(Token::Plus(..)) => {
				// This may either be an addition, like "3+4", or it may be an indexing addressing mode, like "3+X".
				// This can easily be distinguished by trying to parse a right-hand side, and on parse failure not
				// failing, but backtracking the parser to where we were before the "+" and returning the left-hand
				// side. Then, the addressing mode parser can pick up the "+X" again.
				let starting_position = tokens.index;
				let maybe_rhs = self.parse_number(tokens, current_global_label);
				if let Ok(rhs) = maybe_rhs {
					// TODO: This violates operator precedence front and back.
					Ok(Number::Add(Box::new(lhs), Box::new(rhs)))
				} else {
					// All the misparsing from the right-hand side...
					tokens.move_to(starting_position);
					// ... and the "+".
					tokens.backtrack(1);
					Ok(lhs)
				}
			},
			Ok(Token::Slash(..)) => {
				let rhs = self.parse_number(tokens, current_global_label)?;
				Ok(Number::Divide(Box::new(lhs), Box::new(rhs)))
			},
			Ok(token) => Err(AssemblyError::ExpectedToken {
				expected: Token::Newline(token.source_span().offset().into()),
				actual:   token.clone(),
				location: token.source_span(),
				src:      self.source_code.clone(),
			}),
		}
	}

	fn create_literal<'a>(&'a mut self, token: &'a Token, used_as_address: bool) -> Result<Number, AssemblyError> {
		match token {
			Token::Number(number, ..) => Ok(Number::Literal(*number)),
			Token::Identifier(label, ..) =>
				Ok(Number::Label(Label::Global(self.get_global_label(label, token.source_span(), used_as_address)))),
			_ => Err(AssemblyError::ExpectedToken {
				expected: Token::Number(0, token.source_span()),
				actual:   token.clone(),
				location: token.source_span(),
				src:      self.source_code.clone(),
			}),
		}
	}
}
