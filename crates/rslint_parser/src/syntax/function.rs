use crate::parser::ParsedSyntax;
use crate::syntax::binding::parse_binding;
use crate::syntax::decl::parse_parameter_list;
use crate::syntax::js_parse_error;
use crate::syntax::stmt::{is_semi, parse_block_impl};
use crate::syntax::typescript::{ts_type_or_type_predicate_ann, ts_type_params};
use crate::JsSyntaxFeature::TypeScript;
use crate::ParsedSyntax::{Absent, Present};
use crate::{CompletedMarker, ConditionalSyntax, SyntaxFeature};
use crate::{Parser, ParserState};
use rslint_syntax::SyntaxKind::{
	ERROR, JS_FUNCTION_BODY, JS_FUNCTION_DECLARATION, JS_FUNCTION_EXPRESSION, TS_TYPE_ANNOTATION,
};
use rslint_syntax::{SyntaxKind, T};
use std::collections::HashMap;

/// A function declaration, this could be async and or a generator. This takes a marker
/// because you need to first advance over async or start a marker and feed it in.
// test function_decl
// function foo() {}
// function *foo() {}
// async function *foo() {}
// async function foo() {}
// function *foo() {
//   yield foo;
// }
//
// test function_declaration_script
// // SCRIPT
// function test(await) {}
//
// test_err function_decl_err
// function() {}
// function foo {}
// function {}
// function *() {}
// async function() {}
// async function *() {}
// function *foo() {}
// yield foo;
// function test(): number {}
// function foo(await) {}
// function foo(yield) {}
pub(super) fn parse_function_declaration(p: &mut Parser) -> ParsedSyntax<ConditionalSyntax> {
	parse_function(p, JS_FUNCTION_DECLARATION)
}

pub(super) fn parse_function_expression(p: &mut Parser) -> ParsedSyntax<ConditionalSyntax> {
	parse_function(p, JS_FUNCTION_EXPRESSION)
}

fn parse_function(p: &mut Parser, kind: SyntaxKind) -> ParsedSyntax<ConditionalSyntax> {
	let m = p.start();

	let mut uses_invalid_syntax =
		kind == JS_FUNCTION_DECLARATION && p.eat(T![declare]) && TypeScript.is_unsupported(p);

	let in_async = is_at_async_function(p, LineBreak::DoNotCheck);
	if in_async {
		p.bump_remap(T![async]);
	} else {
		p.missing();
	}

	p.expect_required(T![function]);

	let in_generator = p.eat_optional(T![*]);
	let guard = &mut *p.with_state(ParserState {
		labels: HashMap::new(),
		in_function: true,
		in_async,
		in_generator,
		..p.state.clone()
	});

	let id = parse_binding(guard);

	if kind == JS_FUNCTION_DECLARATION {
		id.or_missing_with_error(guard, |p, range| {
			p.err_builder(
				"expected a name for the function in a function declaration, but found none",
			)
			.primary(range, "")
		})
	} else {
		id.or_missing(guard)
	};

	let type_parameters =
		TypeScript.parse_exclusive_syntax(guard, parse_ts_parameter_types, |p, marker| {
			p.err_builder("type parameters can only be used in TypeScript files")
				.primary(marker.range(p), "")
		});

	if type_parameters.or_missing(guard).is_err() {
		uses_invalid_syntax = true;
	}

	parse_parameter_list(guard).or_missing_with_error(guard, js_parse_error::expected_parameters);

	let return_type =
		TypeScript.parse_exclusive_syntax(guard, parse_ts_return_type_if_ts, |p, marker| {
			p.err_builder("return types can only be used in TypeScript files")
				.primary(marker.range(p), "")
		});

	if return_type.or_missing(guard).is_err() {
		uses_invalid_syntax = true;
	}

	if kind == JS_FUNCTION_DECLARATION {
		function_body_or_declaration(guard);
	} else {
		function_body(guard).or_missing_with_error(guard, js_parse_error::expected_function_body);
	}

	let function = Present(m.complete(guard, kind));

	if uses_invalid_syntax {
		function.into_invalid()
	} else {
		function.into_valid()
	}
}

pub(super) fn function_body(p: &mut Parser) -> ParsedSyntax<CompletedMarker> {
	let mut guard = p.with_state(ParserState {
		in_constructor: false,
		in_function: true,
		..p.state.clone()
	});

	parse_block_impl(&mut *guard, JS_FUNCTION_BODY)
}

// TODO 1725 This is probably not ideal (same with the `declare` keyword). We should
// use a different AST type for function declarations. For example, a function declaration should
// never have a body but that would be allowed with this approach. Same for interfaces, interface
// methods should never have a body
/// Either parses a typescript declaration body or the function body
pub(super) fn function_body_or_declaration(p: &mut Parser) {
	// omitting the body is allowed in ts
	if p.typescript() && !p.at(T!['{']) && is_semi(p, 0) {
		p.eat(T![;]);
	} else {
		let body = function_body(p);
		if p.state.in_declare {
			match body {
				Present(mut body) => {
					let err = p
						.err_builder(
							"function implementations cannot be given in ambient (declare) contexts",
						)
						.primary(body.range(p), "");

					p.error(err);
					body.change_kind(p, ERROR);
				}
				_ => {
					p.missing();
				}
			}
		} else {
			body.or_missing_with_error(p, js_parse_error::expected_function_body);
		}
	}
}

pub(crate) fn parse_ts_parameter_types(p: &mut Parser) -> ParsedSyntax<CompletedMarker> {
	if p.at(T![<]) {
		Present(ts_type_params(p).unwrap())
	} else {
		Absent
	}
}

pub(crate) fn ts_parameter_types(p: &mut Parser) {
	if p.at(T![<]) {
		if let Some(ref mut ty) = ts_type_params(p) {
			ty.err_if_not_ts(p, "type parameters can only be used in TypeScript files");
		}
	}
}

pub(crate) fn parse_ts_return_type_if_ts(p: &mut Parser) -> ParsedSyntax<CompletedMarker> {
	if p.at(T![:]) {
		let return_type = p.start();
		if let Some(ref mut ty) = ts_type_or_type_predicate_ann(p, T![:]) {
			ty.err_if_not_ts(p, "return types can only be used in TypeScript files");
		}
		Present(return_type.complete(p, TS_TYPE_ANNOTATION))
	} else {
		Absent
	}
}

/// Tells [is_at_async_function] if it needs to check line breaks
#[derive(PartialEq)]
#[repr(u8)]
pub(super) enum LineBreak {
	// check line breaks
	DoCheck,
	// do not check line break
	DoNotCheck,
}

#[inline]
/// Checks if the parser is inside a "async function"
pub(super) fn is_at_async_function(p: &mut Parser, should_check_line_break: LineBreak) -> bool {
	let async_function_tokens = p.cur_src() == "async" && p.nth_at(1, T![function]);
	if should_check_line_break == LineBreak::DoCheck {
		async_function_tokens && !p.has_linebreak_before_n(1)
	} else {
		async_function_tokens
	}
}
