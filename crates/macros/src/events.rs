//! `#[cordis::events]`（M6，G12）：typed 事件面。
//!
//! # 两条硬约束
//! 1. **事件名逐字**（§0.3 取代清单⑦）：`#[event("tools/before-invoke")]` 产出的就是
//!    这一个字符串——宏绝不加前缀、不改大小写。ABI 侧的拦截名是硬编码的；
//! 2. **载荷走 JSON 信封**：`Dyn::new(ExternValue(json))`，与宿主 `json_to_args` 同一形态
//!    （A9：typed 监听器与动态库监听器看到同一份载荷）。

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    Attribute, Error, Fields, Ident, ItemEnum, Lit, LitStr, Meta, Token, Type,
};

use crate::attr;

pub fn events(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    attr::expect_empty(args, "cordis::events")?;
    let item: ItemEnum = syn::parse2(item)?;
    if !item.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &item.generics,
            "事件枚举不能有泛型参数（每个变体自己带载荷类型）",
        ));
    }

    let enum_ident = item.ident.clone();
    let mut names: Vec<String> = Vec::new();
    let mut consts: Vec<TokenStream> = Vec::new();
    let mut helpers: Vec<TokenStream> = Vec::new();
    let mut variants: Vec<TokenStream> = Vec::new();

    for variant in &item.variants {
        let (event_name, mode) = event_attr(variant.attrs.iter())?;
        let payload: Type = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                fields.unnamed.first().expect("刚数过是一个").ty.clone()
            }
            other => {
                return Err(Error::new_spanned(
                    other,
                    "事件变体必须正好带一个载荷类型：`PreStep(StepReport)`",
                ))
            }
        };
        let variant_ident = &variant.ident;
        let const_ident = attr::screaming_snake(variant_ident);
        let dispatch_ident = format_ident!("{}", attr::snake_case(variant_ident));
        let listener_ident = format_ident!("on_{}", attr::snake_case(variant_ident));

        names.push(event_name.clone());
        consts.push(quote! {
            #[doc = concat!("事件名（逐字）：`", #event_name, "`")]
            pub const #const_ident: &'static str = #event_name;
        });
        helpers.push(dispatch_helper(
            &dispatch_ident,
            &const_ident,
            &payload,
            mode,
            &event_name,
        ));
        helpers.push(listener_helper(
            &listener_ident,
            &const_ident,
            &payload,
            mode,
        ));
        // 变体本身只是声明面（运行期走 JSON），但 `#[cfg]`/`#[doc]` 等属性必须保留；
        // 只丢掉给这个宏吃的 `#[event(…)]`（否则编译器会报 "cannot find attribute"）。
        let variant_attrs: Vec<_> = variant
            .attrs
            .iter()
            .filter(|attribute| !attribute.path().is_ident("event"))
            .collect();
        variants.push(quote!(#(#variant_attrs)* #variant_ident(#payload)));
    }

    let attrs = &item.attrs;
    let vis = &item.vis;

    Ok(quote! {
        #(#attrs)*
        // 枚举只是声明面：运行期载荷走 JSON，变体不会被构造。
        #[allow(dead_code)]
        #vis enum #enum_ident {
            #(#variants),*
        }

        impl #enum_ident {
            #(#consts)*
            #(#helpers)*
        }
    })
}

// ------------------------------------------------------------------ 模式

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Emit,
    Parallel,
    Serial,
    Bail,
    Waterfall,
}

impl Mode {
    fn parse(text: &str) -> Option<Mode> {
        Some(match text {
            "emit" => Mode::Emit,
            "parallel" => Mode::Parallel,
            "serial" => Mode::Serial,
            "bail" => Mode::Bail,
            "waterfall" => Mode::Waterfall,
            _ => return None,
        })
    }

    /// 内核侧的派发入口名（`Ctx::emit` / `Ctx::bail` / …）。
    fn dispatch_fn(self) -> Ident {
        Ident::new(
            match self {
                Mode::Emit => "emit",
                Mode::Parallel => "parallel",
                Mode::Serial => "serial",
                Mode::Bail => "bail",
                Mode::Waterfall => "waterfall",
            },
            proc_macro2::Span::call_site(),
        )
    }

    /// 派发入口的返回类型（`emit` 不要返值，其余带回 `Ret`）。
    fn returns_ret(self) -> bool {
        self != Mode::Emit
    }

    fn doc(self) -> &'static str {
        match self {
            Mode::Emit => "观察：监听器都跑，返回值丢弃",
            Mode::Parallel => "并行观察：监听器并发跑，返回值丢弃",
            Mode::Serial => "顺序观察：一个一个跑，遇到第一个表态即返回",
            Mode::Bail => "短路：第一个表态的监听器赢",
            Mode::Waterfall => "环绕中间件：下游可改写载荷，链尾的决定回流",
        }
    }
}

/// `#[event("name", mode = "bail")]`。
struct EventArgs {
    name: String,
    mode: Mode,
}

impl Parse for EventArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: LitStr = input.parse()?;
        let mut mode = Mode::Emit;
        while input.peek(Token![,]) {
            let _: Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }
            let meta: Meta = input.parse()?;
            match &meta {
                Meta::NameValue(pair) if pair.path.is_ident("mode") => {
                    let lit = match &pair.value {
                        syn::Expr::Lit(literal) => match &literal.lit {
                            Lit::Str(text) => text,
                            _ => {
                                return Err(Error::new_spanned(
                                    &pair.value,
                                    "`mode` 需要字符串字面量",
                                ))
                            }
                        },
                        other => return Err(Error::new_spanned(other, "`mode` 需要字符串字面量")),
                    };
                    mode = Mode::parse(&lit.value()).ok_or_else(|| {
                        Error::new_spanned(
                            lit,
                            "`mode` 只认 emit / parallel / serial / bail / waterfall",
                        )
                    })?;
                }
                other => {
                    return Err(Error::new_spanned(
                        other,
                        "只认 `mode = \"…\"` 一个参数（事件名是第一个位置参数）",
                    ))
                }
            }
        }
        Ok(EventArgs {
            name: name.value(),
            mode,
        })
    }
}

fn event_attr<'a>(attrs: impl Iterator<Item = &'a Attribute>) -> syn::Result<(String, Mode)> {
    let mut found = None;
    for attribute in attrs {
        if !attribute.path().is_ident("event") {
            continue;
        }
        if found.is_some() {
            return Err(Error::new_spanned(
                attribute,
                "一个变体只能有一个 `#[event(…)]`",
            ));
        }
        let parsed: EventArgs = attribute.parse_args()?;
        found = Some((parsed.name, parsed.mode));
    }
    found.ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "每个事件变体都要写 `#[event(\"逐字事件名\")]`——事件名绝不从变体名推导",
        )
    })
}

// ------------------------------------------------------------------ 生成物

fn dispatch_helper(
    ident: &Ident,
    event_const: &Ident,
    payload: &Type,
    mode: Mode,
    event: &str,
) -> TokenStream {
    let dispatch_fn = mode.dispatch_fn();
    let doc = format!("派发 `{event}`（{}）。", mode.doc());
    let returns = if mode.returns_ret() {
        quote!(::cordis::core::Ret)
    } else {
        quote!(())
    };
    let empty = if mode.returns_ret() {
        quote!(::std::boxed::Box::pin(async {
            ::core::option::Option::None
        }))
    } else {
        quote!(::std::boxed::Box::pin(async {}))
    };
    let tail = if mode == Mode::Waterfall {
        quote!(tail: ::core::option::Option<::cordis::core::BoxFuture<::cordis::core::Ret>>,)
    } else {
        quote!()
    };
    let tail_arg = if mode == Mode::Waterfall {
        quote!(args, tail, ::core::option::Option::None)
    } else {
        quote!(args)
    };
    quote! {
        #[doc = #doc]
        pub fn #ident(
            ctx: &::cordis::core::Ctx,
            payload: #payload,
            #tail
        ) -> ::cordis::core::BoxFuture<#returns> {
            match ::cordis::typed::events::args_or_log(ctx, Self::#event_const, &payload) {
                ::core::option::Option::Some(args) => ctx.#dispatch_fn(Self::#event_const, #tail_arg),
                ::core::option::Option::None => #empty,
            }
        }
    }
}

fn listener_helper(ident: &Ident, event_const: &Ident, payload: &Type, mode: Mode) -> TokenStream {
    let event = mode.doc();
    let handler_bound = match mode {
        Mode::Emit => quote! {
            F: ::core::ops::Fn(#payload) -> ::cordis::core::BoxFuture<
                ::core::result::Result<(), ::cordis::core::Error>
            > + ::core::marker::Send + ::core::marker::Sync + 'static
        },
        Mode::Waterfall => quote! {
            F: ::core::ops::Fn(
                #payload,
                ::cordis::typed::events::TypedNext<#payload>,
            ) -> ::cordis::core::BoxFuture<
                ::core::result::Result<::core::option::Option<#payload>, ::cordis::core::Error>
            > + ::core::marker::Send + ::core::marker::Sync + 'static
        },
        _ => quote! {
            F: ::core::ops::Fn(#payload) -> ::cordis::core::BoxFuture<
                ::core::result::Result<::core::option::Option<#payload>, ::cordis::core::Error>
            > + ::core::marker::Send + ::core::marker::Sync + 'static
        },
    };
    let handler_call = if mode == Mode::Waterfall {
        quote!(handler(payload, ::cordis::typed::events::TypedNext::new(call.next, Self::#event_const)))
    } else {
        quote!(handler(payload))
    };
    let settle = match mode {
        // `emit`：监听器的返回值本来就被丢弃，所以只看成功/失败（失败记日志，不炸宿主）。
        Mode::Emit => quote! {
            if let ::core::result::Result::Err(err) = fut.await {
                log_ctx.log(::cordis::core::Level::Error, &err.to_string());
            }
            ::core::option::Option::None
        },
        // 其余模式：`Ok(Some(payload))` 就是"表态"，编成 JSON 信封返回。
        _ => quote! {
            match fut.await {
                ::core::result::Result::Ok(::core::option::Option::Some(payload)) => {
                    match ::cordis::typed::events::encode_payload(Self::#event_const, &payload) {
                        ::core::result::Result::Ok(encoded) => ::core::option::Option::Some(encoded),
                        ::core::result::Result::Err(err) => {
                            log_ctx.log(::cordis::core::Level::Error, &err.to_string());
                            ::core::option::Option::None
                        }
                    }
                }
                ::core::result::Result::Ok(::core::option::Option::None) => ::core::option::Option::None,
                ::core::result::Result::Err(err) => {
                    log_ctx.log(::cordis::core::Level::Error, &err.to_string());
                    ::core::option::Option::None
                }
            }
        },
    };
    let doc = format!(
        "注册一个 typed 监听器（{}；由调用者 fiber 拥有，卸载即摘除）。",
        event
    );
    quote! {
        #[doc = #doc]
        pub fn #ident<F>(ctx: &::cordis::core::Ctx, handler: F) -> ::cordis::core::Disposable
        where
            #handler_bound
        {
            ctx.on(Self::#event_const, move |call: ::cordis::core::Call| {
                let log_ctx = call.ctx.clone();
                let payload = match ::cordis::typed::events::decode_payload::<#payload>(
                    &call.args,
                    Self::#event_const,
                ) {
                    ::core::result::Result::Ok(payload) => payload,
                    ::core::result::Result::Err(err) => {
                        log_ctx.log(::cordis::core::Level::Error, &err.to_string());
                        return ::std::boxed::Box::pin(async { ::core::option::Option::None });
                    }
                };
                let fut = #handler_call;
                ::std::boxed::Box::pin(async move {
                    #settle
                })
            })
        }
    }
}
