//! `#[cordis::service]` / `#[cordis::service_impl]`（M6，D2）。
//!
//! # 脱糖的三件事（D2 的"四个必须先修的缺陷"里前两条）
//! 1. `async fn` 不是 `dyn` 兼容的 ⇒ 脱糖成 `fn m<'a>(&'a self, …) -> ServiceFuture<'a, …>`
//!    （借用盒：`dyn_value.rs` 的 `BoxFuture` 是 `'static` 的，装不下借用 `&self` 的 future）；
//! 2. `dyn Trait` 要 `Send + Sync` ⇒ **重写 trait 头**，补上这两个超 trait；
//! 3. `Dyn::new(Arc<dyn T>)` 会得到 `Arc<Arc<dyn T>>` ⇒ 值形态是 `Iface<dyn T>`（内核侧，M6）。
//!
//! # 编译期拒绝的事（不静默降级）
//! 非 `async fn`、接收者不是 `&self`、方法泛型、trait/impl 带泛型参数——一律报错。
//! "参数/返回值必须能走 JSON"由生成的适配器在编译期兜住（`DeserializeOwned` / `Serialize`
//! 的 bound 会在展开处点出类型名）。

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    spanned::Spanned, FnArg, Ident, ItemImpl, ItemTrait, Meta, Pat, PatType, ReturnType, TraitItem,
    Type,
};

use crate::attr;

pub fn service(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let item: ItemTrait = syn::parse2(item)?;
    let (name, version) = parse_args(args, &item.ident)?;
    expand_service(item, name, version)
}

pub fn service_impl(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    attr::expect_empty(args, "cordis::service_impl")?;
    expand_impl(syn::parse2(item)?)
}

// ------------------------------------------------------------------ 参数

fn parse_args(args: TokenStream, ident: &Ident) -> syn::Result<(String, u32)> {
    let mut name = attr::snake_case(ident);
    let mut version = 1;
    for meta in attr::metas(args)? {
        match &meta {
            Meta::NameValue(pair) if pair.path.is_ident("name") => name = attr::string_value(pair)?,
            Meta::NameValue(pair) if pair.path.is_ident("version") => {
                version = attr::u32_value(pair)?
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "只认 `name = \"…\"` 与 `version = 1` 两个参数",
                ))
            }
        }
    }
    Ok((name, version))
}

// ------------------------------------------------------------------ 方法

/// 一个服务方法的声明面（trait 侧与 impl 侧共用同一套规则）。
#[derive(Clone)]
struct Method {
    attrs: Vec<syn::Attribute>,
    ident: Ident,
    /// `(参数名, 参数类型)`，按声明顺序。
    args: Vec<(String, Type)>,
    /// 返回类型**原样**（`Result<T, E>`）。
    output: Type,
}

impl Method {
    fn parse(sig: &syn::Signature) -> syn::Result<Method> {
        let span = sig.span();
        if sig.asyncness.is_none() {
            return Err(syn::Error::new(
                span,
                format!(
                    "服务方法 `{}` 必须是 `async fn`（跨 ABI 的调用点只能 await）",
                    sig.ident
                ),
            ));
        }
        if !sig.generics.params.is_empty() {
            return Err(syn::Error::new(
                span,
                "服务方法不能有泛型参数（trait 必须 object-safe）",
            ));
        }
        let mut inputs = sig.inputs.iter();
        match inputs.next() {
            Some(FnArg::Receiver(receiver)) => {
                if !matches!(receiver.kind, syn::ReceiverKind::Reference(_, None, None)) {
                    return Err(syn::Error::new_spanned(
                        receiver,
                        "服务方法的接收者必须是 `&self`（服务对象跨 fiber 共享，不能独占、也不加生命周期）",
                    ));
                }
            }
            Some(other) => {
                return Err(syn::Error::new_spanned(
                    other,
                    format!("服务方法 `{}` 的第一个参数必须是 `&self`", sig.ident),
                ))
            }
            None => {
                return Err(syn::Error::new(
                    span,
                    format!("服务方法 `{}` 缺少 `&self` 接收者", sig.ident),
                ))
            }
        }
        let mut args = Vec::new();
        for input in inputs {
            let FnArg::Typed(PatType { pat, ty, .. }) = input else {
                return Err(syn::Error::new_spanned(
                    input,
                    "服务方法只能有一个 `&self` 接收者",
                ));
            };
            let Pat::Ident(pattern) = &**pat else {
                return Err(syn::Error::new_spanned(
                    pat,
                    "服务方法的参数必须是简单名字（`who: String`），不能是解构/通配模式",
                ));
            };
            args.push((pattern.ident.to_string(), (**ty).clone()));
        }
        let ReturnType::Type(_, output) = &sig.output else {
            return Err(syn::Error::new(
                span,
                format!(
                    "服务方法 `{}` 必须声明返回类型，且是 `Result<T, E>`（`E` 要能 `Display`）",
                    sig.ident
                ),
            ));
        };
        Ok(Method {
            attrs: Vec::new(),
            ident: sig.ident.clone(),
            args,
            output: (**output).clone(),
        })
    }

    /// 脱糖后的签名（trait 声明与 impl 定义必须逐字一致，否则 impl 对不上 trait）。
    fn signature(&self) -> TokenStream {
        let Method {
            ident,
            args,
            output,
            ..
        } = self;
        let params = args.iter().map(|(name, ty)| {
            let name = Ident::new(name, ty.span());
            quote!(#name: #ty)
        });
        quote! {
            fn #ident<'async_trait>(&'async_trait self, #(#params),*)
                -> ::cordis::core::ServiceFuture<'async_trait, #output>
        }
    }
}

// ------------------------------------------------------------------ trait 侧

fn expand_service(item: ItemTrait, name: String, version: u32) -> syn::Result<TokenStream> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "服务 trait 不能有泛型参数（注册键是具体类型，`dyn Trait` 也要求它具体）",
        ));
    }
    let trait_ident = item.ident.clone();
    let companion = format_ident!("{}Service", trait_ident);
    let adapter = format_ident!("{}JsonAdapter", trait_ident);
    let key = format!("{name}@{version}");
    let vis = item.vis.clone();

    let mut methods: Vec<Method> = Vec::new();
    let mut emitted: Vec<TokenStream> = Vec::new();
    for entry in &item.items {
        match entry {
            TraitItem::Fn(function) => {
                let mut method = Method::parse(&function.sig)?;
                method.attrs = function.attrs.clone();
                let attrs = &method.attrs;
                let signature = method.signature();
                emitted.push(match &function.default {
                    // trait 里的默认实现同样脱糖：否则默认方法借不到 `&self`。
                    Some(body) => quote! {
                        #(#attrs)*
                        #signature {
                            ::std::boxed::Box::pin(async move #body)
                        }
                    },
                    None => quote!(#(#attrs)* #signature;),
                });
                methods.push(method);
            }
            other => emitted.push(quote!(#other)),
        }
    }

    // trait 头：补 `Send + Sync`（`dyn Trait` 要跨线程派发、还要能进 `Dyn`）。
    let mut bounds: Vec<TokenStream> = item
        .supertraits
        .iter()
        .map(|bound| quote!(#bound))
        .collect();
    let declared = |wanted: &str| {
        item.supertraits.iter().any(|existing| match existing {
            syn::TypeParamBound::Trait(trait_bound) => trait_bound.path.is_ident(wanted),
            _ => false,
        })
    };
    if !declared("Send") {
        bounds.push(quote!(::core::marker::Send));
    }
    if !declared("Sync") {
        bounds.push(quote!(::core::marker::Sync));
    }

    let attrs = &item.attrs;
    let names: Vec<String> = methods
        .iter()
        .map(|method| method.ident.to_string())
        .collect();

    let arms = methods.iter().map(|method| {
        let method_name = method.ident.to_string();
        let arity = method.args.len();
        let open = if arity == 0 {
            // 零参数方法不开游标：免得生成一个没用的绑定（也会招来 unused 警告）。
            quote!(::cordis::typed::service::open(#companion::NAME, method.as_str(), args, 0)?;)
        } else {
            quote!(let mut envelope = ::cordis::typed::service::open(#companion::NAME, method.as_str(), args, #arity)?;)
        };
        let takes = method.args.iter().enumerate().map(|(index, (arg_name, arg_type))| {
            let bind = format_ident!("a{index}");
            quote!(let #bind: #arg_type = envelope.take::<#arg_type>(#arg_name)?;)
        });
        let binds: Vec<Ident> = (0..arity).map(|index| format_ident!("a{index}")).collect();
        let call = &method.ident;
        quote! {
            #method_name => {
                #open
                #(#takes)*
                let value = inner.#call(#(#binds),*)
                    .await
                    .map_err(|err| ::cordis::typed::service::returned_error(
                        #companion::NAME, method.as_str(), err,
                    ))?;
                ::cordis::typed::service::reply(#companion::NAME, method.as_str(), value)
            }
        }
    });

    Ok(quote! {
        #(#attrs)*
        #vis trait #trait_ident: #(#bounds)+* {
            #(#emitted)*
        }

        /// 由 `#[cordis::service]` 生成的伴生类型：服务的键、访问器与注册入口。
        ///
        /// 名字是 trait 名 + `Service`；与既有类型撞名时改 trait 名即可。
        #vis struct #companion;

        impl #companion {
            /// 服务名（不含版本）。
            pub const NAME: &'static str = #name;
            /// 接口的 major 版本。
            pub const MAJOR: u32 = #version;
            /// 注册键：`<name>@<major>`（§6.6 路线 1）。
            pub const KEY: &'static str = #key;

            /// typed 访问器：**T1 专用**（`TypeId` 不跨编译单元稳定；T2/T3 走 `Ctx::call_iface`）。
            pub fn of(
                ctx: &::cordis::core::Ctx,
            ) -> ::core::result::Result<::std::sync::Arc<dyn #trait_ident>, ::cordis::core::ResolveError> {
                ctx.iface::<dyn #trait_ident>(Self::KEY)
            }

            /// 把一个实现注册成这个服务：typed 面与 JSON 面指向**同一份实现**
            /// （D2 的 T1/T2 规则：一个 key 在实现域内只能有一个实现）。
            pub fn provide<T: #trait_ident + 'static>(
                ctx: &::cordis::core::Ctx,
                value: ::std::sync::Arc<T>,
            ) -> ::cordis::core::Result<::cordis::core::Disposable> {
                let typed: ::std::sync::Arc<dyn #trait_ident> = value;
                let json = Self::adapter(typed.clone());
                ctx.provide(
                    Self::KEY,
                    ::cordis::core::Dyn::new(::cordis::core::Iface::<dyn #trait_ident>(typed))
                        .with_adapter(json),
                )
            }

            /// JSON 面适配器（T2/T3 调用者与 `Ctx::call_iface` 走它）。
            pub fn adapter(
                inner: ::std::sync::Arc<dyn #trait_ident>,
            ) -> ::std::sync::Arc<dyn ::cordis::core::ExternService> {
                ::std::sync::Arc::new(#adapter { inner })
            }
        }

        /// typed 服务的 JSON 面（由 `#[cordis::service]` 生成）。
        ///
        /// 与 typed 面持有**同一个** `Arc`：两条通道打到同一份实现。
        /// `caller_meta`（M3 的提供者侧元数据 `d(…) ⊕ ι(…)`）在这里没有落点——typed 方法的
        /// 签名里没有它的位置。需要按调用方切换策略的提供者应当手写 `ExternService`，
        /// 或在调用侧用 `Ctx::intercept_ctx` 携带配置。
        struct #adapter {
            inner: ::std::sync::Arc<dyn #trait_ident>,
        }

        impl ::cordis::core::ExternService for #adapter {
            fn service(&self) -> &str {
                #companion::NAME
            }

            fn version(&self) -> u32 {
                #companion::MAJOR
            }

            fn methods(&self) -> ::std::vec::Vec<::std::string::String> {
                ::std::vec![#(#names.to_string()),*]
            }

            fn call(
                &self,
                _caller_meta: ::core::option::Option<::cordis::core::Config>,
                method: ::std::string::String,
                args: ::cordis::core::Config,
            ) -> ::cordis::core::BoxFuture<::cordis::core::Result<::cordis::core::Config>> {
                let inner = self.inner.clone();
                ::std::boxed::Box::pin(async move {
                    match method.as_str() {
                        #(#arms)*
                        other => ::core::result::Result::Err(::cordis::typed::service::unknown_method(
                            #companion::NAME,
                            other,
                            &[#(#names),*],
                        )),
                    }
                })
            }
        }
    })
}

// ------------------------------------------------------------------ impl 侧

fn expand_impl(item: ItemImpl) -> syn::Result<TokenStream> {
    let trait_path = item
        .trait_
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(&item.self_ty, "只能用在 `impl Trait for Type` 上"))?
        .0
        .clone();
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "服务 impl 不能带泛型参数（与 `#[cordis::service]` 的 trait 侧同一限制）",
        ));
    }
    let self_ty = item.self_ty.clone();
    let mut emitted: Vec<TokenStream> = Vec::new();
    for entry in &item.items {
        match entry {
            syn::ImplItem::Fn(function) => {
                let method = Method::parse(&function.sig)?;
                let signature = method.signature();
                let docs = &function.attrs;
                let block = &function.block;
                emitted.push(quote! {
                    #(#docs)*
                    #signature {
                        ::std::boxed::Box::pin(async move #block)
                    }
                });
            }
            other => emitted.push(quote!(#other)),
        }
    }
    Ok(quote! {
        impl #trait_path for #self_ty {
            #(#emitted)*
        }
    })
}
