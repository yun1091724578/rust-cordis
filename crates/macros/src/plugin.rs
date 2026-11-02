//! `#[cordis::plugin]`（M6）：把插件的**声明**收进语法树，生成 `impl Plugin`。
//!
//! # 为什么必须要求 `Clone`
//! `Plugin::apply` 返回 `'static` future（`dyn_value.rs` 的 `BoxFuture`），而 trait 方法的
//! 接收者是 `&self`——借用活不过这个盒子。宏因此在调用前克隆一份（"一次 `Arc<Self>` 提升"的
//! 廉价版本）：**共享状态放 `Arc`**，与既有示例/夹具的写法一致。
//!
//! # 为什么 `apply` 是固定钩子名
//! 生成的是 `impl ::cordis::core::Plugin for T`，作者写的是固有方法
//! `async fn apply(&self, ctx, config) -> Result<()>`。Rust 的路径解析里**固有项优先**，
//! 所以 `T::apply(...)` 命中作者写的那一个（trait 那一个同名方法就在同一个 impl 里，
//! 但它是候选列表里的第二个）。

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemStruct, Meta, Type};

use crate::attr;

fn validate_schema(text: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("必须是合法 JSON：{err}"))?;
    if !value.is_object() {
        return Err("必须是 JSON 对象".to_string());
    }
    Ok(())
}

pub fn plugin(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let item: ItemStruct = syn::parse2(item)?;
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "插件类型不能带泛型参数（注册表身份键要求它是具体类型）",
        ));
    }
    let ident = item.ident.clone();
    let mut name = ident.to_string();
    let mut key: Option<String> = None;
    let mut inject: Vec<TokenStream> = Vec::new();
    let mut provision: Vec<TokenStream> = Vec::new();
    let mut volatile: Vec<TokenStream> = Vec::new();
    let mut schema: Option<String> = None;
    let mut config: Option<Type> = None;

    for meta in attr::metas(args)? {
        match &meta {
            Meta::NameValue(pair) if pair.path.is_ident("name") => name = attr::string_value(pair)?,
            Meta::NameValue(pair) if pair.path.is_ident("key") => {
                key = Some(attr::string_value(pair)?)
            }
            Meta::NameValue(pair) if pair.path.is_ident("schema") => {
                let text = attr::string_value(pair)?;
                validate_schema(&text).map_err(|message| {
                    syn::Error::new_spanned(pair, format!("schema = \"…\" {message}"))
                })?;
                schema = Some(text);
            }
            Meta::List(list) if list.path.is_ident("inject") => inject = attr::list_entries(list)?,
            Meta::List(list) if list.path.is_ident("provision") => {
                provision = attr::list_entries(list)?
            }
            Meta::List(list) if list.path.is_ident("volatile") => {
                volatile = attr::list_entries(list)?
            }
            Meta::List(list) if list.path.is_ident("config") => {
                config = Some(list.parse_args::<Type>()?);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "只认 `name = \"…\"` / `key = \"…\"` / `schema = \"…\"` / `inject(…)` / \
                     `provision(…)` / `volatile(…)` / `config(Type)`",
                ))
            }
        }
    }

    // `key = "…"` 给了才覆盖默认（默认实现 = 按显示名共享，见 `Plugin::key`）。
    let key_fn = key.map(|key| {
        quote! {
            fn key(&self) -> ::cordis::core::PluginKey {
                ::std::sync::Arc::from(#key)
            }
        }
    });
    // `schema = "…"` 在宏展开期校验是 JSON 对象；它仍是展示用声明，
    // 真正的配置校验由 `config(Type)` / 插件自己负责。
    let schema_fn = schema.map(|schema| {
        quote! {
            fn config_schema(&self) -> ::core::option::Option<::std::string::String> {
                ::core::option::Option::Some((#schema).to_string())
            }
        }
    });

    // `config(Type)` 给了就把校验接到挂载期（`Store::resolve_config` 会先问 `validate`）。
    let validate_fn = config.map(|config| {
        quote! {
            fn validate(
                &self,
                raw: ::cordis::core::Config,
            ) -> ::core::result::Result<::cordis::core::Config, ::cordis::core::ValidationError> {
                <#config>::parse(raw.clone())?;
                ::core::result::Result::Ok(raw)
            }
        }
    });

    // 把固有 `apply` 的调用放进一个**不导入 `Plugin` trait** 的子模块。
    // 若作者忘记写固有 `apply`，`#ident::apply` 在这里不会回退到正在生成的
    // `Plugin::apply`（否则每次挂载都会递归一层），而是直接编译失败。
    let apply_module = format_ident!("__cordis_plugin_apply_{}", ident);
    Ok(quote! {
        #item

        #[doc(hidden)]
        #[allow(non_snake_case)]
        mod #apply_module {
            use super::#ident;

            pub(super) fn call(
                this: #ident,
                ctx: ::cordis::core::Ctx,
                config: ::cordis::core::Config,
            ) -> ::cordis::core::BoxFuture<::cordis::core::Result<()>> {
                ::std::boxed::Box::pin(async move { #ident::apply(&this, ctx, config).await })
            }
        }

        impl ::cordis::core::Plugin for #ident {
            fn name(&self) -> &str {
                #name
            }

            #key_fn

            fn inject(&self) -> ::cordis::core::Inject {
                ::cordis::core::Inject::of(&[#(#inject),*])
            }

            fn provision(&self) -> ::std::vec::Vec<::std::string::String> {
                ::std::vec![#(#provision.to_string()),*]
            }

            fn volatile_paths(&self) -> ::std::vec::Vec<::std::string::String> {
                ::std::vec![#(#volatile.to_string()),*]
            }

            #schema_fn

            #validate_fn

            /// 入口：接上 `impl #ident` 里那个固有 `async fn apply`。
            ///
            /// 先克隆一份 `self`：`Plugin::apply` 的 future 是 `'static` 的，
            /// 借来的 `&self` 活不过它（见宏文档的 `Clone` 契约）。
            fn apply(
                &self,
                ctx: ::cordis::core::Ctx,
                config: ::cordis::core::Config,
            ) -> ::cordis::core::BoxFuture<::cordis::core::Result<()>> {
                let this = ::std::clone::Clone::clone(self);
                #apply_module::call(this, ctx, config)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::validate_schema;

    #[test]
    fn schema_must_be_a_json_object() {
        assert!(validate_schema(r#"{"type":"object"}"#).is_ok());
        assert!(validate_schema("[]").unwrap_err().contains("JSON 对象"));
        assert!(validate_schema("not-json")
            .unwrap_err()
            .contains("合法 JSON"));
    }
}
