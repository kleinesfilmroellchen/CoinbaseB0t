//! SPC700 assembler.
//!
//! spcasm is both a library and two binaries: spcasm and brr.

#![allow(stable_features)]
#![feature(
	test,
	result_flattening,
	is_some_and,
	iterator_try_collect,
	if_let_guard,
	int_log,
	get_mut_unchecked,
	iter_intersperse,
	const_option_ext,
	const_for,
	let_chains,
	option_result_contains,
	slice_as_chunks,
	exact_size_is_empty,
	maybe_uninit_uninit_array,
	const_trait_impl,
	drain_filter,
	maybe_uninit_array_assume_init
)]
#![allow(non_upper_case_globals)]

#[macro_use] extern crate lalrpop_util;
#[macro_use] extern crate lazy_static;

#[allow(clippy::wildcard_imports)]
pub use common::*;
pub use segments::Segments;

/// Just like -Werror on C(++) compilers, make ALL THE WARNINGS INTO ERRORS!
#[macro_export]
macro_rules! w_error {
	($vis:vis mod $modname:ident) => {
		#[deny(missing_docs, unused, clippy::all, clippy::pedantic, clippy::nursery)]
		$vis mod $modname;
	};
}

w_error!(pub mod assembler);
w_error!(pub mod brr);
w_error!(pub mod cli);
w_error!(mod common);
w_error!(mod default_hacks);
w_error!(mod directive);
#[cfg(feature = "binaries")]
w_error!(pub mod elf);
w_error!(mod error);
w_error!(mod lalrpop_adaptor);
w_error!(pub mod parser);
w_error!(mod segments);

lalrpop_mod!(
	#[allow(missing_docs, unused, clippy::all, clippy::pedantic, clippy::nursery)]
	asm
);

#[cfg(feature = "binaries")]
shadow_rs::shadow!(buildinfo);

#[cfg(test)]
w_error!(mod test);

#[cfg(feature = "binaries")]
w_error!(mod spcasm);

#[cfg(feature = "binaries")]
#[allow(unused)]
fn main() -> miette::Result<()> {
	spcasm::main()
}
