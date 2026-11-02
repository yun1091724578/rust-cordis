//! `#[cordis::config]`（M6）：typed 配置。
//!
//! 生成 serde 派生 + 两个方法：
//! - `parse(raw) -> Result<Self, ValidationError>`：失败是内核的 `invalid config:` 形状，
//!   于是配置错误在**挂载期**被报出来（配合 `#[cordis::plugin(config(Type))]`）；
//! - `to_config(&self) -> Config`：序列化回 JSON（volatile 快照、日志、测试往返都用得上）。
//!
//! serde 的路径经 `::cordis::serde` 走（sdk 门面再导出），所以用户 crate 只需要一条依赖。

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use crate::attr;

pub fn config(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    attr::expect_empty(args, "cordis::config")?;
    let item: ItemStruct = syn::parse2(item)?;
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "配置类型不能带泛型参数（它要被 JSON 解出来）",
        ));
    }
    let ident = item.ident.clone();
    let named: Vec<(String, syn::Type)> = match &item.fields {
        syn::Fields::Named(fields) => fields
            .named
            .iter()
            .filter_map(|field| {
                field
                    .ident
                    .as_ref()
                    .map(|ident| (ident.to_string(), field.ty.clone()))
            })
            .collect(),
        _ => Vec::new(),
    };
    // 逐字段体检（只在 serde 已经失败之后跑）：`from_value` 的错误里没有字段名（没有路径
    // 信息），而"哪个字段错了"是配置诊断最要紧的一半。第一个解不出来的字段进 issues。
    let field_probes = named.iter().map(|(name, ty)| {
        quote! {
            if issues.is_empty() {
                if let ::core::option::Option::Some(value) = fields.get(#name) {
                    if let ::core::result::Result::Err(field_err) =
                        ::cordis::serde_json::from_value::<#ty>(value.clone())
                    {
                        issues.push(::std::format!("{}: {field_err}", #name));
                    }
                }
            }
        }
    });
    let scan = if named.is_empty() {
        quote!()
    } else {
        quote! {
            if let ::cordis::core::Config::Object(fields) = &raw {
                #(#field_probes)*
            }
        }
    };
    Ok(quote! {
        #[derive(::cordis::serde::Serialize, ::cordis::serde::Deserialize)]
        #[serde(crate = "::cordis::serde")]
        #item

        impl #ident {
            /// 从 `Config`（JSON）解析。失败给 `ValidationError`——与 JS 的
            /// `invalid config:\n  - …` 同一形状，因此能在挂载期被报出来。
            ///
            /// 诊断优先给**字段级**的那一条（serde 自己的消息里没有字段名）；
            /// 字段级扫描说不出话来时（比如"缺字段"）才用 serde 的原话。
            pub fn parse(
                raw: ::cordis::core::Config,
            ) -> ::core::result::Result<Self, ::cordis::core::ValidationError> {
                match ::cordis::serde_json::from_value::<Self>(raw.clone()) {
                    ::core::result::Result::Ok(value) => ::core::result::Result::Ok(value),
                    ::core::result::Result::Err(err) => {
                        let mut issues: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
                        #scan
                        if issues.is_empty() {
                            issues.push(err.to_string());
                        }
                        ::core::result::Result::Err(::cordis::core::ValidationError { issues })
                    }
                }
            }

            /// 序列化回 `Config`。
            pub fn to_config(&self) -> ::cordis::core::Config {
                // 普通配置类型不会序列化失败；真失败了也**不能**把插件打挂，
                // 退回 `Null` 并让读取方看到"空"（诊断靠 `parse` 那一侧的报错）。
                ::cordis::serde_json::to_value(self)
                    .unwrap_or(::cordis::serde_json::Value::Null)
            }
        }
    })
}
