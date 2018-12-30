use crate::{helpers::Helpers, util::ExprFactory};
use ast::*;
use serde::{Deserialize, Serialize};
use std::{
    iter, mem,
    sync::{atomic::Ordering, Arc},
};
use swc_atoms::JsWord;
use swc_common::{
    errors::{ColorConfig, Handler},
    sync::Lrc,
    FileName, Fold, FoldWith, SourceMap, Spanned, DUMMY_SP,
};
use swc_ecma_parser::{Parser, Session, SourceFileInput, Syntax};

#[cfg(test)]
mod tests;

#[derive(Debug, Serialize, Deserialize)]
pub struct Options {
    #[serde(default = "default_pragma")]
    pub pragma: String,
    #[serde(default = "default_pragma_frag")]
    pub pragma_frag: String,

    #[serde(default = "default_throw_if_namespace")]
    pub throw_if_namespace: bool,

    #[serde(default)]
    pub development: bool,

    #[serde(default)]
    pub use_builtins: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            pragma: default_pragma(),
            pragma_frag: default_pragma_frag(),
            throw_if_namespace: default_throw_if_namespace(),
            development: false,
            use_builtins: false,
        }
    }
}

fn default_pragma() -> String {
    "React.createElement".into()
}

fn default_pragma_frag() -> String {
    "React.Fragment".into()
}

fn default_throw_if_namespace() -> bool {
    true
}

/// `@babel/plugin-transform-react-jsx`
///
/// Turn JSX into React function calls
pub fn jsx(cm: Lrc<SourceMap>, options: Options, helpers: Arc<Helpers>) -> impl Fold<Module> {
    let handler = Handler::with_tty_emitter(ColorConfig::Always, false, true, Some(cm.clone()));

    let session = Session {
        cfg: Default::default(),
        handler: &handler,
    };
    let parse = |name, s| {
        let fm = cm.new_source_file(FileName::Custom(format!("<jsx-config-{}.js>", name)), s);

        Parser::new(session, Syntax::Es2019, SourceFileInput::from(&*fm))
            .parse_expr()
            .unwrap()
    };

    Jsx {
        pragma: ExprOrSuper::Expr(parse("pragma", options.pragma)),
        pragma_frag: ExprOrSpread {
            spread: None,
            expr: parse("pragma_frag", options.pragma_frag),
        },
        use_builtins: options.use_builtins,
        helpers,
    }
}

struct Jsx {
    pragma: ExprOrSuper,
    pragma_frag: ExprOrSpread,
    use_builtins: bool,
    helpers: Arc<Helpers>,
}

impl Jsx {
    fn jsx_frag_to_expr(&mut self, el: JSXFragment) -> Expr {
        let span = el.span();

        Expr::Call(CallExpr {
            span,
            callee: self.pragma.clone(),
            args: iter::once(self.pragma_frag.clone())
                // attribute: null
                .chain(iter::once(Lit::Null(Null { span: DUMMY_SP }).as_arg()))
                .chain({
                    // Children
                    el.children
                        .into_iter()
                        .filter_map(|c| self.jsx_elem_child_to_expr(c))
                })
                .collect(),
        })
    }

    fn jsx_elem_to_expr(&mut self, el: JSXElement) -> Expr {
        let span = el.span();

        let name = jsx_name(el.opening.name);

        Expr::Call(CallExpr {
            span,
            callee: self.pragma.clone(),
            args: iter::once(name.as_arg())
                .chain(iter::once({
                    // Attributes
                    self.fold_attrs(el.opening.attrs).as_arg()
                }))
                .chain({
                    // Children
                    el.children
                        .into_iter()
                        .filter_map(|c| self.jsx_elem_child_to_expr(c))
                })
                .collect(),
        })
    }

    fn jsx_elem_child_to_expr(&mut self, c: JSXElementChild) -> Option<ExprOrSpread> {
        Some(match c {
            JSXElementChild::JSXText(text) => {
                // TODO(kdy1): Optimize
                let s = Str {
                    span: text.span,
                    has_escape: text.raw != text.value,
                    value: jsx_text_to_str(text.value),
                };
                if s.value.is_empty() {
                    return None;
                }
                Lit::Str(s).as_arg()
            }
            JSXElementChild::JSXExprContainer(JSXExprContainer {
                expr: JSXExpr::Expr(e),
            }) => e.as_arg(),
            JSXElementChild::JSXExprContainer(JSXExprContainer {
                expr: JSXExpr::JSXEmptyExpr(..),
            }) => return None,
            JSXElementChild::JSXElement(el) => self.jsx_elem_to_expr(*el).as_arg(),
            JSXElementChild::JSXFragment(el) => self.jsx_frag_to_expr(el).as_arg(),
            JSXElementChild::JSXSpreadChild(JSXSpreadChild { .. }) => {
                unimplemented!("jsx sperad child")
            }
        })
    }

    fn fold_attrs(&mut self, attrs: Vec<JSXAttrOrSpread>) -> Box<Expr> {
        if attrs.is_empty() {
            return box Expr::Lit(Lit::Null(Null { span: DUMMY_SP }));
        }

        let is_complex = attrs.iter().any(|a| match *a {
            JSXAttrOrSpread::SpreadElement(..) => true,
            _ => false,
        });

        if is_complex {
            let mut args = vec![];
            let mut cur_obj_props = vec![];
            macro_rules! check {
                () => {{
                    if args.is_empty() || !cur_obj_props.is_empty() {
                        args.push(
                            ObjectLit {
                                span: DUMMY_SP,
                                props: mem::replace(&mut cur_obj_props, vec![]),
                            }
                            .as_arg(),
                        )
                    }
                }};
            }
            for attr in attrs {
                match attr {
                    JSXAttrOrSpread::JSXAttr(a) => {
                        cur_obj_props.push(PropOrSpread::Prop(box attr_to_prop(a)))
                    }
                    JSXAttrOrSpread::SpreadElement(e) => {
                        check!();
                        args.push(e.expr.as_arg());
                    }
                }
            }
            check!();

            // calls `_extends` or `Object.assign`
            box Expr::Call(CallExpr {
                span: DUMMY_SP,
                callee: {
                    if self.use_builtins {
                        member_expr!(DUMMY_SP, Object.assign).as_callee()
                    } else {
                        self.helpers.extends.store(true, Ordering::Relaxed);
                        quote_ident!("_extends").as_callee()
                    }
                },
                args,
            })
        } else {
            box Expr::Object(ObjectLit {
                span: DUMMY_SP,
                props: attrs
                    .into_iter()
                    .map(|a| match a {
                        JSXAttrOrSpread::JSXAttr(a) => a,
                        _ => unreachable!(),
                    })
                    .map(attr_to_prop)
                    .map(Box::new)
                    .map(PropOrSpread::Prop)
                    .collect(),
            })
        }
    }
}

impl Fold<Expr> for Jsx {
    fn fold(&mut self, expr: Expr) -> Expr {
        let expr = expr.fold_children(self);

        match expr {
            Expr::Paren(ParenExpr {
                expr: box Expr::JSXElement(el),
                ..
            })
            | Expr::JSXElement(el) => {
                // <div></div> => React.createElement('div', null);
                self.jsx_elem_to_expr(el)
            }
            _ => expr,
        }
    }
}

fn jsx_name(name: JSXElementName) -> Box<Expr> {
    let span = name.span();
    match name {
        JSXElementName::Ident(i) => {
            // If it starts with lowercase digit
            let c = i.sym.chars().next().unwrap();

            if &*i.sym == "this" {
                return box Expr::This(ThisExpr { span });
            }

            if c.is_ascii_lowercase() {
                box Expr::Lit(Lit::Str(Str {
                    span,
                    value: i.sym,
                    has_escape: false,
                }))
            } else {
                box Expr::Ident(i)
            }
        }
        JSXElementName::JSXNamespacedName(JSXNamespacedName { ref ns, ref name }) => {
            box Expr::Lit(Lit::Str(Str {
                span,
                value: format!("{}:{}", ns.sym, name.sym).into(),
                has_escape: false,
            }))
        }
        JSXElementName::JSXMemberExpr(JSXMemberExpr { obj, prop }) => {
            fn convert_obj(obj: JSXObject) -> ExprOrSuper {
                let span = obj.span();

                match obj {
                    JSXObject::Ident(i) => {
                        if &*i.sym == "this" {
                            return ExprOrSuper::Expr(box Expr::This(ThisExpr { span }));
                        }
                        i.as_obj()
                    }
                    JSXObject::JSXMemberExpr(box JSXMemberExpr { obj, prop }) => MemberExpr {
                        span,
                        obj: convert_obj(obj),
                        prop: box Expr::Ident(prop),
                        computed: false,
                    }
                    .as_obj(),
                }
            }
            box Expr::Member(MemberExpr {
                span,
                obj: convert_obj(obj),
                prop: box Expr::Ident(prop),
                computed: false,
            })
        }
    }
}

fn attr_to_prop(a: JSXAttr) -> Prop {
    let key = to_prop_name(a.name);
    let value = a.value.unwrap_or_else(|| {
        box Expr::Lit(Lit::Bool(Bool {
            span: key.span(),
            value: true,
        }))
    });
    Prop::KeyValue(KeyValueProp { key, value })
}

fn to_prop_name(n: JSXAttrName) -> PropName {
    let span = n.span();

    match n {
        JSXAttrName::Ident(i) => {
            if i.sym.contains("-") {
                PropName::Str(Str {
                    span,
                    value: i.sym,
                    has_escape: false,
                })
            } else {
                PropName::Ident(i)
            }
        }
        JSXAttrName::JSXNamespacedName(JSXNamespacedName { ns, name }) => PropName::Str(Str {
            span,
            value: format!("{}:{}", ns.sym, name.sym).into(),
            has_escape: false,
        }),
    }
}

fn jsx_text_to_str(t: JsWord) -> JsWord {
    if !t.contains(' ') && !t.contains('\n') {
        return t;
    }

    let mut buf = String::new();
    for s in t.replace("\n", " ").split_ascii_whitespace() {
        buf.push_str(s);
        buf.push(' ');
    }

    buf.into()
}
