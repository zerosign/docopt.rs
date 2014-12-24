#![crate_name = "docopt_macros"]
#![crate_type = "dylib"]
#![feature(plugin_registrar, macro_rules, quote)]

//! This crate defines the `docopt!` macro. It is documented in the
//! documentation of the `docopt` crate.

extern crate syntax;
extern crate rustc;
extern crate docopt;

use std::collections::HashMap;

use rustc::plugin::Registry;
use syntax::ast;
use syntax::codemap;
use syntax::ext::base::{ExtCtxt, MacResult, MacItems, DummyResult};
use syntax::ext::build::AstBuilder;
use syntax::fold::Folder;
use syntax::owned_slice::OwnedSlice;
use syntax::parse::common::SeqSep;
use syntax::parse::parser::Parser;
use syntax::parse::token;
use syntax::print::pprust;
use syntax::ptr::P;

use docopt::{Docopt, ArgvMap};
use docopt::parse::{Options, Atom, Positional, Zero, One};

#[plugin_registrar]
pub fn plugin_registrar(reg: &mut Registry) {
    reg.register_macro("docopt", expand);
}

fn expand(cx: &mut ExtCtxt, span: codemap::Span, tts: &[ast::TokenTree])
         -> Box<MacResult+'static> {
    let parsed = match MacParser::new(cx, tts).parse() {
        Ok(parsed) => parsed,
        Err(_) => return DummyResult::any(span),
    };
    parsed.items(cx)
}

/// Parsed corresponds to the result of parsing a `docopt` macro call.
/// It can be used to write a corresponding struct.
struct Parsed {
    struct_info: StructInfo,
    doc: Docopt,
    /// Overrided type annotations for struct members. May be empty.
    /// When a type annotation for an atom doesn't exist, then one is
    /// inferred automatically. It is one of: `bool`, `uint`, `String` or
    /// `Vec<String>`.
    types: HashMap<Atom, P<ast::Ty>>,
}

impl Parsed {
    /// Returns a macro result suitable for expansion.
    /// Contains two items: one for the struct and one for the struct impls.
    fn items(&self, cx: &ExtCtxt) -> Box<MacResult+'static> {
        let mut its = vec!();
        its.push(self.struct_decl(cx));

        let struct_name = self.struct_info.name;
        let full_doc = self.doc.parser().full_doc.as_slice();
        its.push(quote_item!(cx,
            impl $struct_name {
                #[allow(dead_code)]
                fn docopt() -> docopt::Docopt {
                    // The unwrap is justified here because this code
                    // gen only happens if the Docopt usage string is valid.
                    docopt::Docopt::new($full_doc).unwrap()
                }
            }
        ).unwrap());
        MacItems::new(its.into_iter())
    }

    /// Returns an item for the struct definition.
    fn struct_decl(&self, cx: &ExtCtxt) -> P<ast::Item> {
        let name = self.struct_info.name.clone();
        let vis = if self.struct_info.public { ast::Public }
                  else { ast::Inherited };
        let def = ast::StructDef {
            fields: self.struct_fields(cx),
            ctor_id: None
        };

        let mut traits = vec!["RustcDecodable".to_string()];
        traits.push_all(self.struct_info.deriving.as_slice());
        let attrs = vec![attribute(cx, "allow", vec!["non_snake_case"]),
                         attribute(cx, "deriving", traits)];

        let st = cx.item_struct(codemap::DUMMY_SP, name.clone(), def);
        cx.item(codemap::DUMMY_SP, name, attrs, st.node.clone()).map(|mut it| {
            it.vis = vis;
            it
        })
    }

    /// Returns a list of fields for the struct definition.
    /// Handles type annotations.
    fn struct_fields(&self, cx: &ExtCtxt) -> Vec<ast::StructField> {
        let mut fields: Vec<ast::StructField> = vec!();
        for (atom, opts) in self.doc.parser().descs.iter() {
            let name = ArgvMap::key_to_struct_field(atom.to_string().as_slice());
            let ty = match self.types.get(atom) {
                None => self.pat_type(cx, atom, opts),
                Some(ty) => ty.clone(),
            };
            fields.push(self.mk_struct_field(name.as_slice(), ty));
        }
        fields
    }

    /// Returns an inferred type for a usage pattern.
    /// This is only invoked when a type annotation is not present.
    fn pat_type(&self, cx: &ExtCtxt, atom: &Atom, opts: &Options) -> P<ast::Ty> {
        let sp = codemap::DUMMY_SP;
        match (opts.repeats, &opts.arg) {
            (false, &Zero) => {
                match atom {
                    &Positional(_) => cx.ty_ident(sp, ident("String")),
                    _ => cx.ty_ident(sp, ident("bool")),
                }
            }
            (true, &Zero) => {
                match atom {
                    &Positional(_) => ty_vec_string(cx),
                    _ => cx.ty_ident(sp, ident("uint")),
                }
            }
            (false, &One(_)) => cx.ty_ident(sp, ident("String")),
            (true, &One(_)) => ty_vec_string(cx),
        }
    }

    /// Creates a struct field from a member name and type.
    fn mk_struct_field(&self, name: &str, ty: P<ast::Ty>) -> ast::StructField {
        codemap::dummy_spanned(ast::StructField_ {
            kind: ast::NamedField(ident(name), ast::Public),
            id: ast::DUMMY_NODE_ID,
            ty: ty,
            attrs: vec!(),
        })
    }
}

/// State for parsing a `docopt` macro invocation.
struct MacParser<'a, 'b:'a> {
    cx: &'a mut ExtCtxt<'b>,
    p: Parser<'b>,
}

impl<'a, 'b> MacParser<'a, 'b> {
    fn new(cx: &'a mut ExtCtxt<'b>, tts: &[ast::TokenTree]) -> MacParser<'a, 'b> {
        let p = cx.new_parser_from_tts(tts);
        MacParser { cx: cx, p: p }
    }

    /// Main entry point for parsing arguments to `docopt` macro.
    /// First looks for an identifier for the struct name.
    /// Second, a string containing the docopt usage patterns.
    /// Third, an optional list of type annotations.
    fn parse(&mut self) -> Result<Parsed, ()> {
        if self.p.token == token::Eof {
            self.cx.span_err(self.cx.call_site(), "macro expects arguments");
            return Err(());
        }
        let struct_info = try!(self.parse_struct_info());
        let docstr = try!(self.parse_str());

        let sep = SeqSep {
            sep: Some(token::Comma),
            trailing_sep_allowed: true,
        };
        let types = self.p.parse_seq_to_end(
            &token::Eof, sep, |p| MacParser::parse_type_annotation(p)
        ).into_iter()
         .map(|(ident, ty)| {
             let field_name = token::get_ident(ident).to_string();
             let key = ArgvMap::struct_field_to_key(field_name.as_slice());
             (Atom::new(key.as_slice()), ty)
          })
         .collect::<HashMap<Atom, P<ast::Ty>>>();
        self.p.expect(&token::Eof);

        // This config does not matter because we're only asking for the
        // usage patterns in the Docopt string. The configuration does not
        // affect the retrieval of usage patterns.
        let doc = match Docopt::new(docstr) {
            Ok(doc) => doc,
            Err(err) => {
                self.cx.span_err(self.cx.call_site(),
                                 format!("Invalid Docopt usage: {}",
                                         err).as_slice());
                return Err(());
            }
        };
        Ok(Parsed {
            struct_info: struct_info,
            doc: doc,
            types: types,
        })
    }

    /// Parses a single string literal. On failure, an error is logged and
    /// unit is returned.
    fn parse_str(&mut self) -> Result<String, ()> {
        fn lit_is_str(lit: &ast::Lit) -> bool {
            match lit.node {
                ast::LitStr(_, _) => true,
                _ => false,
            }
        }
        fn lit_to_string(lit: &ast::Lit) -> String {
            match lit.node {
                ast::LitStr(ref s, _) => s.to_string(),
                _ => panic!("BUG: expected string literal"),
            }
        }
        let exp = self.cx.expander().fold_expr(self.p.parse_expr());
        let s = match exp.node {
            ast::ExprLit(ref lit) if lit_is_str(&**lit) => {
                lit_to_string(&**lit)
            }
            _ => {
                let err = format!("Expected string literal but got {}",
                                  pprust::expr_to_string(&*exp));
                self.cx.span_err(exp.span, err.as_slice());
                return Err(());
            }
        };
        self.p.bump();
        Ok(s)
    }

    /// Parses a type annotation in a `docopt` invocation of the form
    /// `ident: Ty`.
    /// Note that this is a static method as it is used as a HOF.
    fn parse_type_annotation(p: &mut Parser) -> (ast::Ident, P<ast::Ty>) {
        let ident = p.parse_ident();
        p.expect(&token::Colon);
        let ty = p.parse_ty();
        (ident, ty)
    }

    /// Parses struct information, like visibility, name and deriving.
    fn parse_struct_info(&mut self) -> Result<StructInfo, ()> {
        let public = self.p.eat_keyword(token::keywords::Pub);
        let mut info = StructInfo {
            name: self.p.parse_ident(),
            public: public,
            deriving: vec![],
        };
        if self.p.eat(&token::Comma) { return Ok(info); }
        let deriving = self.p.parse_ident();
        if deriving.as_str() != "deriving" {
            let err = format!("Expected 'deriving' keyword but got '{}'",
                              deriving);
            self.cx.span_err(self.cx.call_site(), err.as_slice());
            return Err(());
        }
        while !self.p.eat(&token::Comma) {
            info.deriving.push(self.p.parse_ident().as_str().to_string());
        }
        Ok(info)
    }
}

struct StructInfo {
    name: ast::Ident,
    public: bool,
    deriving: Vec<String>,
}

// Convenience functions for building intermediate values.

fn ident(s: &str) -> ast::Ident {
    ast::Ident::new(token::intern(s))
}

fn attribute<S, T>(cx: &ExtCtxt, name: S, items: Vec<T>) -> ast::Attribute
            where S: Str, T: Str {
    let sp = codemap::DUMMY_SP;
    let its = items.into_iter().map(|s| meta_item(cx, s.as_slice())).collect();
    let mi = cx.meta_list(sp, intern(name.as_slice()), its);
    cx.attribute(sp, mi)
}

fn meta_item(cx: &ExtCtxt, s: &str) -> P<ast::MetaItem> {
    cx.meta_word(codemap::DUMMY_SP, intern(s))
}

fn intern(s: &str) -> token::InternedString {
    token::intern_and_get_ident(s)
}

fn ty_vec_string(cx: &ExtCtxt) -> P<ast::Ty> {
    let sp = codemap::DUMMY_SP;
    let tystr = ast::AngleBracketedParameterData {
        lifetimes: vec![],
        types: OwnedSlice::from_vec(vec![cx.ty_ident(sp, ident("String"))]),
        bindings: OwnedSlice::empty(),
    };
    cx.ty_path(ast::Path {
        span: sp,
        global: false,
        segments: vec![ast::PathSegment {
            identifier: ident("Vec"),
            parameters: ast::PathParameters::AngleBracketedParameters(tystr),
        }]
    })
}
