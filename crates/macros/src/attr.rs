//! 属性参数的解析，以及那几条**只做字面解释**的小规则。
//!
//! 这些规则只在这里实现一次（别处不许再抄一遍）：
//!
//! | 写法 | 解出来的东西 |
//! |---|---|
//! | `inject(tools)` | `"tools"`（裸标识符当字符串） |
//! | `inject("tools@1")` | `"tools@1"`（字面量原样用——**命名空间的写法**） |
//! | `inject(TOOLS_KEY)` | 按路径求值（拿到 `&'static str`，方便与 `#[cordis::service]` 的 `KEY` 对齐） |
//!
//! 为什么裸标识符当字符串：`inject(tools, llm)` 读起来就是声明本身，而 `tools` 这个名字
//! 在作者的作用域里通常**并不存在**（它是服务的键，不是变量）。要写命名空间就写字面量：
//! 宏不替你猜 major（那是 `#[cordis::service]` 的 `KEY` 的职责）。

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, ExprLit, Ident, Lit, Meta, Token};

/// `inject(…)` / `provision(…)` / `volatile(…)` 里的一个条目 → 一个 `&'static str` 表达式。
pub fn entry(expr: &Expr) -> syn::Result<TokenStream> {
    match expr {
        // 裸标识符 ⇒ 字符串；但 `SCREAMING_SNAKE` 约定是常量，按路径求值
        //（与 `#[cordis::service]` 生成的 `KEY` 对齐）。
        Expr::Path(expr_path)
            if expr_path.qself.is_none() && expr_path.path.get_ident().is_some() =>
        {
            let ident = expr_path.path.get_ident().expect("上面刚检查过");
            let text = ident.to_string();
            let looks_like_constant = text
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
            if looks_like_constant {
                let path = &expr_path.path;
                Ok(quote!(#path))
            } else {
                Ok(quote!(#text))
            }
        }
        // 字面量 ⇒ 原样。
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Ok(quote!(#text)),
        // 其余 ⇒ 按表达式求值（路径常量、`concat!` 之类）。
        other => Ok(quote!(#other)),
    }
}

/// 解析一串逗号分隔的条目（`inject(a, "b@1")` 的括号里那一段）。
pub fn entries(tokens: TokenStream) -> syn::Result<Vec<TokenStream>> {
    let list = syn::parse::Parser::parse(
        syn::punctuated::Punctuated::<Expr, Token![,]>::parse_terminated,
        tokens.into(),
    )?;
    list.iter().map(entry).collect()
}

/// 取一个字符串字面量参数（`name = "…"`）。
pub fn string_value(meta: &syn::MetaNameValue) -> syn::Result<String> {
    match &meta.value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Ok(text.value()),
        other => Err(syn::Error::new_spanned(other, "这里需要字符串字面量")),
    }
}

/// 取一个整数参数（`version = 1`）。
pub fn u32_value(meta: &syn::MetaNameValue) -> syn::Result<u32> {
    match &meta.value {
        Expr::Lit(ExprLit {
            lit: Lit::Int(number),
            ..
        }) => number.base10_parse::<u32>(),
        other => Err(syn::Error::new_spanned(
            other,
            "这里需要整数（接口的 major 版本）",
        )),
    }
}

/// 只接受空参数（`#[cordis::events]` / `#[cordis::config]` 不接受参数）。
pub fn expect_empty(args: TokenStream, macro_name: &str) -> syn::Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        args,
        format!("`{macro_name}` 不接受参数（声明都写在属性与条目上）"),
    ))
}

/// 把 `CamelCase` 转成 `snake_case`（trait 名 → 默认服务名的用）。
pub fn snake_case(ident: &Ident) -> String {
    let text = ident.to_string();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (index, ch) in chars.iter().enumerate() {
        if ch.is_uppercase() {
            let prev = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let next = chars.get(index + 1).copied();
            // 缩写边界：`HTTPClient` → `http_client`（在 C 前断开），
            // 但 `HTTP` 内部不断开；`LLM` → `llm`。
            let boundary = index != 0
                && (prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
                    || (prev.is_some_and(|p| p.is_uppercase())
                        && next.is_some_and(|n| n.is_lowercase())));
            if boundary {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(*ch);
        }
    }
    out
}

/// 把 `CamelCase` 转成 `SCREAMING_SNAKE_CASE`（变体名 → 事件名常量名的用）。
pub fn screaming_snake(ident: &Ident) -> Ident {
    Ident::new(&snake_case(ident).to_uppercase(), ident.span())
}

/// 解析 `#[…(a, b, name = "x")]` 形式的一整串参数。
pub fn metas(args: TokenStream) -> syn::Result<Vec<Meta>> {
    let list = syn::parse::Parser::parse(
        syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated,
        args.into(),
    )?;
    Ok(list.into_iter().collect())
}

/// 便利：`Meta::List` 的条目（`inject(…)` 的括号内容）。
pub fn list_entries(list: &syn::MetaList) -> syn::Result<Vec<TokenStream>> {
    entries(list.tokens.clone())
}
