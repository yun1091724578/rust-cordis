//! `!!js` 表达式：**内置求值器**（M4-4a）。
//!
//! # 为什么是"子集"而不是"不做"
//! cordis 的配置是 YAML，而 `!!js` 让 YAML 里能写 JavaScript。
//! 上游跑在 Node 里，一行 `new Function` 就把全量语义给了它（`loader/src/config/utils.ts:5-9`）。
//! 本仓跑在 Rust 里：把整个 JS 引擎塞进来不是"补全"，而是换一个工程——
//! 所以这里做的是**明确的子集 + 明确的拒绝**：
//!
//! | 支持 | 形态 |
//! |---|---|
//! | 字面量 | 数字 / 字符串（单双引号、转义、`\uXXXX`）/ `true` / `false` / `null` / `undefined` |
//! | 命名空间 | `process.platform`、`process.version`、`process.cwd()`、`process.env.*` |
//! | 服务 | `ctx.get('<名>')`、`ctx.<名>`（等价写法）、`?.`、`??` |
//! | 函数 | `Number(x)`、`String(x)`、`Boolean(x)`、`JSON.parse(<字符串>)` |
//! | 运算 | `!` `-` `+`、`+ - * / %`、`== != === !==`、`< <= > >=`、`&& || ??`、`三元`、括号 |
//! | 成员 | `.length`（字符串/数组）、对象取键、`[下标]` |
//!
//! 明确**拒绝**（一律报错，绝不静默成 `null` / 默认值）：
//! - `process.getBuiltinModule('node:path')…`（出厂 bundle 真实形态）、模板串、
//!   数组/对象字面量、`spread`、IIFE；
//! - **裸标识符**：上游 `with (ctx)` 会去 ctx 上找 `dshHomePath(...)` 这种名字，
//!   本仓拒绝——猜一个服务名比报错危险得多；
//! - 字符串/数组方法（`split` / `includes` / `startsWith` …）与 `JSON.stringify`；
//! - `NaN` / `Infinity` 这类**不可表达为 JSON 的值**（`1/0` 在 JS 里是 `Infinity`，
//!   在这里是错误——把 `Infinity` 悄悄写成 `null` 才是灾难）。
//!
//! 需要全量的宿主往 `LoaderConfig.js_evaluator` 里注入真引擎即可（挂点覆盖内置子集）。
//!
//! # 值模型
//! 求值期的值只有三种来源：**JSON 值**、**内建命名空间**（`process`/`ctx`/…，
//! 它们不是数据，只是成员与调用的起点）、以及**短路标记**（`a?.b.c` 里 `a` 为
//! nullish 时，整条链在 JS 里都不会再求值）。落到配置里时短路标记收敛成 `null`。
//!
//! 已知的两处口径（附录 A 记录）：
//! 1. `null` 与 `undefined` 在配置里都是 `null`（JSON 没有 `undefined`）——
//!    真值、`??`、`==` 的结果都与 JS 一致，唯一能看出差别的地方是字符串拼接
//!    （`'a' + undefined` 得 `"aundefined"`）；
//! 2. 对**对象/数组**做 `===` 是结构比较（JS 里对象相等是身份相等，本仓的值没有身份），
//!    对它们做 `==` / 大小比较则是**错误**（JS 的 `ToPrimitive` 规则不适合猜）。

use cordis_core::{Config, Ctx, ExternValue};
use serde_json::Number;

/// `!!js` 表达式的求值器（可插拔挂点；内置实现是 [`BuiltinJs`]）。
///
/// # 为什么是 trait 而不是只有内置引擎
/// 出厂 bundle 里有 `process.getBuiltinModule('node:path').resolve(…)`、模板串、
/// `dshHomePath(…)` 这类形态——完整 JS 语义只能靠真引擎（§0 的"会红清单"）。
/// 所以"求值"是一等能力：宿主注入引擎即获全量，不注入则用内置子集；
/// 两者都不认的形态**按行 fail loud**（绝不静默降级成默认值）。
///
/// # 为什么必须"每次决策现算"
/// 上游 `Entry.disabled` 是 getter（`entry.ts:76-85`）：每次读都沿祖先链重新求值，
/// 于是 `disabled: !!js "!ctx.get('profileContext')"` 会跟着上下文走。
/// 缓存求值结果会让一行永远停在第一次的答案上——那是配置在撒谎。
pub trait JsEvaluator: Send + Sync {
    /// 求值一个表达式，返回值按 JSON 形态收敛（[`Config`]）。
    ///
    /// `ctx` 是**声明这一行的上下文**：祖先链上的 isolate/intercept 已经叠加，
    /// 出厂写法 `ctx.get('profileContext')` 读的就是它。
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> std::result::Result<Config, String>;
}

/// 内置子集求值器（[模块文档](self)里有完整清单）。
///
/// 无状态、可共享：每次 `evaluate` 都从文本重新解析（表达式很短，缓存不值它的复杂度，
/// 而"配置里的表达式永远是当下这份文本"这条性质比省几次解析值钱）。
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinJs;

impl JsEvaluator for BuiltinJs {
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> std::result::Result<Config, String> {
        let tokens = tokenize(expr)?;
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
            depth: 0,
        };
        let node = parser.parse_expr()?;
        parser.expect_end()?;
        finish(eval(&node, ctx)?)
    }
}

/// JS 的 `Boolean(x)`——`disabled` 位用的就是它（上游 `Boolean(this.evaluate(...))`）。
///
/// 与 Rust 的直觉不同：**空串 / 0 / null 是假，`{}` 与 `[]` 恒为真**。
/// 这一条不能"大概对"，它就是 `disabled` 的语义定义。
pub fn truthy(value: &Config) -> bool {
    match value {
        Config::Null => false,
        Config::Bool(item) => *item,
        Config::Number(item) => item
            .as_f64()
            .map(|number| number != 0.0 && !number.is_nan())
            .unwrap_or(true),
        Config::String(item) => !item.is_empty(),
        Config::Array(_) | Config::Object(_) => true,
    }
}

// ---------------------------------------------------------------- 词法

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Str(String),
    Ident(String),
    Punct(&'static str),
}

/// 标点按**最长优先**匹配：`?.` 必须在 `.` 之前，`===` 必须在 `==` 之前。
///
/// `{`/`}` 也进表：子集不做对象字面量，但让**解析器**给出"不支持对象字面量"，
/// 比在词法层说"不认识的字符"有用得多。
const PUNCTS: &[&str] = &[
    "?.", "===", "!==", "==", "!=", "<=", ">=", "&&", "||", "??", "+", "-", "*", "/", "%", "!",
    "<", ">", "(", ")", "[", "]", "{", "}", ",", "?", ":", ".",
];

fn tokenize(expr: &str) -> std::result::Result<Vec<Token>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // 注释：块标量里的表达式常常带一行说明，上游 `eval` 也接受它们。
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                j += 1;
            }
            if j + 1 >= chars.len() {
                return Err("块注释没有闭合（`/*` 缺 `*/`）".into());
            }
            i = j + 2;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            let value: f64 = text
                .parse()
                .map_err(|_| format!("不是合法的数字字面量: `{text}`"))?;
            tokens.push(Token::Number(value));
            continue;
        }
        if c == '\'' || c == '"' {
            let (text, next) = read_string(&chars, i)?;
            tokens.push(Token::Str(text));
            i = next;
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let rest: String = chars[i..].iter().take(3).collect();
        match PUNCTS.iter().find(|punct| rest.starts_with(**punct)) {
            Some(punct) => {
                tokens.push(Token::Punct(punct));
                i += punct.chars().count();
            }
            None => return Err(format!("不认识的字符 `{c}`")),
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------- 能力快照（M9，D5）

/// 一段 `!!js` 表达式**静态**读到的宿主面（M9，D5）。
///
/// 这是"装载前审阅"的原料：在信任一份配置之前，要能一眼看出它会去读哪些环境变量、
/// 依赖哪些宿主事实、通过 `ctx.get` 摸到哪些服务。
///
/// # 只有引用，没有值（D5 的红线）
/// `env` 里放的是**变量名**；`process.env.API_KEY` 的**内容**绝不进快照——
/// 能力清单会被写进日志、报告与 CLI 输出，凭据值进去就等于泄漏。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JsCapabilities {
    /// 读到的环境变量名（`process.env.X` / `env.X`），排序去重。
    pub env: Vec<String>,
    /// 用到的宿主事实（`process.platform` / `process.version` / `process.cwd`），排序去重。
    pub host: Vec<String>,
    /// `ctx.get('<name>')` 引用的服务名，排序去重。
    pub services: Vec<String>,
}

impl JsCapabilities {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.host.is_empty() && self.services.is_empty()
    }

    /// 合并另一份快照（一行里有多段表达式，或整棵树汇总）。
    pub fn absorb(&mut self, other: JsCapabilities) {
        self.env.extend(other.env);
        self.host.extend(other.host);
        self.services.extend(other.services);
    }

    /// 收口：排序 + 去重（快照要能逐字断言）。
    pub fn normalize(&mut self) {
        for list in [&mut self.env, &mut self.host, &mut self.services] {
            list.sort();
            list.dedup();
        }
    }

    /// JSON 形态（装载报告与 CLI 用；键名稳定）。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "env": self.env, "host": self.host, "services": self.services })
    }
}

/// 静态扫描一段表达式，抽出它**可能**读到的宿主面。
///
/// # 为什么是词法扫描而不是"求值一遍看看"
/// 能力面必须在**求值之前**就能说清楚：`a ? process.env.X : 1` 走哪个分支取决于运行环境，
/// 但 `X` 始终是这份配置要求的东西——审阅要的是"它可能碰到什么"，不是"这一次碰到了什么"。
///
/// # 认得出的形态（与内置子集的成员访问一一对应）
/// `process.env.X` / `env.X` / `process.X` / `ctx.get('X')`，`.` 与 `?.` 等价。
/// 认不出（语法错、词法错、`ctx.get(变量)`）⇒ 空快照，**不在这里报错**：
/// 报错是求值那条路的事，抢戏会让同一条表达式出现两种错误。
///
/// 已知的过报（宁可多报，不可漏报）：`foo.env.X` 这种"别的对象上的 env 成员"
/// 也会记进 `env`——子集里没有对象字面量，这条路径实际上走不通。
pub fn capabilities_of(expr: &str) -> JsCapabilities {
    let mut caps = JsCapabilities::default();
    let Ok(tokens) = tokenize(expr) else {
        return caps;
    };
    let mut index = 0;
    while index < tokens.len() {
        if let Token::Ident(head) = &tokens[index] {
            match head.as_str() {
                "process" => {
                    if let Some((member, after)) = member_at(&tokens, index + 1) {
                        if member == "env" {
                            if let Some((name, _)) = member_at(&tokens, after)
                                .or_else(|| index_string_at(&tokens, after))
                            {
                                caps.env.push(name);
                            }
                        } else {
                            caps.host.push(member);
                        }
                    }
                }
                // 裸 `env.X` / `env['X']`（子集也认这个形态；`process.env.X`
                // 会在上面被记一次，去重兜住）。
                "env" => {
                    if let Some((name, _)) = member_at(&tokens, index + 1)
                        .or_else(|| index_string_at(&tokens, index + 1))
                    {
                        caps.env.push(name);
                    }
                }
                "ctx" => {
                    if let Some((member, after)) = member_at(&tokens, index + 1) {
                        if member == "get" && matches!(tokens.get(after), Some(Token::Punct("("))) {
                            if let Some(Token::Str(name)) = tokens.get(after + 1) {
                                caps.services.push(name.clone());
                            }
                        } else if member != "get" {
                            // `ctx.x` 与 `ctx.get('x')` 求值等价；能力快照不能漏报。
                            caps.services.push(member);
                        }
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    caps.normalize();
    caps
}

/// `tokens[at]` 是 `.`/`?.` 且紧跟标识符 ⇒ `(成员名, 该标识符之后的游标)`。
fn member_at(tokens: &[Token], at: usize) -> Option<(String, usize)> {
    match tokens.get(at) {
        Some(Token::Punct(punct)) if *punct == "." || *punct == "?." => {}
        _ => return None,
    }
    match tokens.get(at + 1) {
        Some(Token::Ident(name)) => Some((name.clone(), at + 2)),
        _ => None,
    }
}

/// `tokens[at]` 是 `[` 且紧跟字符串 ⇒ `(键名, `]` 之后的游标)`。
fn index_string_at(tokens: &[Token], at: usize) -> Option<(String, usize)> {
    if !matches!(tokens.get(at), Some(Token::Punct("["))) {
        return None;
    }
    match (tokens.get(at + 1), tokens.get(at + 2)) {
        (Some(Token::Str(name)), Some(Token::Punct("]"))) => Some((name.clone(), at + 3)),
        _ => None,
    }
}

/// `interpolation = "injected"` 且宿主**没注入引擎**时生效的求值器（M9，D5）。
///
/// 每个表达式都 fail loud：宿主既然声明"只用注入的引擎"，悄悄回落到内置子集
/// 就是让配置在一个**它没被审阅过**的解释器里跑——这是 D5 里唯一不能让的一步。
/// （同样理由：这里不能返回 `null` 之类的"默认值"，静默降级正是 M1-4 起的红线。）
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectInjected;

impl JsEvaluator for RejectInjected {
    fn evaluate(&self, expr: &str, _ctx: &Ctx) -> std::result::Result<Config, String> {
        Err(format!(
            "interpolation = \"injected\"，但宿主没有注入 js_evaluator：`!!js` 表达式不予求值\
             （原文: {expr}）。注入引擎，或把 interpolation 改回 \"builtin\""
        ))
    }
}

fn read_string(chars: &[char], start: usize) -> std::result::Result<(String, usize), String> {
    let quote = chars[start];
    let mut out = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == quote {
            return Ok((out, i + 1));
        }
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(next) = chars.get(i + 1).copied() else {
            break;
        };
        i += 2;
        match next {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '0' => out.push('\0'),
            'u' => {
                let hex: String = chars
                    .get(i..i + 4)
                    .map(|slice| slice.iter().collect())
                    .unwrap_or_default();
                let code = u32::from_str_radix(&hex, 16)
                    .map_err(|_| format!("`\\u` 转义需要 4 位十六进制，收到 `{hex}`"))?;
                out.push(char::from_u32(code).ok_or_else(|| format!("`\\u{hex}` 不是合法字符"))?);
                i += 4;
            }
            'x' => {
                let hex: String = chars
                    .get(i..i + 2)
                    .map(|slice| slice.iter().collect())
                    .unwrap_or_default();
                let code = u32::from_str_radix(&hex, 16)
                    .map_err(|_| format!("`\\x` 转义需要 2 位十六进制，收到 `{hex}`"))?;
                out.push(char::from_u32(code).ok_or_else(|| format!("`\\x{hex}` 不是合法字符"))?);
                i += 2;
            }
            'v' => out.push('\u{b}'),
            // 行继续符：反斜杠 + 换行不产生字符。
            '\n' => {}
            '\r' => {
                if chars.get(i) == Some(&'\n') {
                    i += 1;
                }
            }
            '\\' | '\'' | '"' | '/' => out.push(next),
            other => return Err(format!("不支持的字符串转义 `\\{other}`")),
        }
    }
    Err("字符串没有闭合".into())
}

// ---------------------------------------------------------------- 语法

enum Node {
    Lit(Config),
    /// 起始标识符（`process` / `ctx` / …；`true`/`null` 之类在解析期就变成 [`Node::Lit`]）。
    Head(String),
    Member {
        base: Box<Node>,
        step: Step,
    },
    Call {
        callee: Box<Node>,
        args: Vec<Node>,
    },
    Unary {
        op: UnOp,
        arg: Box<Node>,
    },
    Binary {
        op: BinOp,
        left: Box<Node>,
        right: Box<Node>,
    },
    Ternary {
        cond: Box<Node>,
        then: Box<Node>,
        els: Box<Node>,
    },
}

enum Step {
    Name { name: String, optional: bool },
    Index { index: Box<Node>, optional: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum UnOp {
    Not,
    Neg,
    Pos,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Coalesce,
}

/// 嵌套深度上限：配置不是敌手，但表达式是外部输入，递归下降不该能被一串括号打穿。
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn eat_punct(&mut self, punct: &str) -> bool {
        if matches!(self.peek(), Some(Token::Punct(found)) if *found == punct) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, punct: &str) -> std::result::Result<(), String> {
        if self.eat_punct(punct) {
            Ok(())
        } else {
            Err(format!("缺少 `{punct}`"))
        }
    }

    fn expect_ident(&mut self, what: &str) -> std::result::Result<String, String> {
        match self.peek() {
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.pos += 1;
                Ok(name)
            }
            Some(token) => Err(format!("{what}需要一个名字，收到 {token:?}")),
            None => Err(format!("{what}需要一个名字，表达式却结束了")),
        }
    }

    fn expect_end(&mut self) -> std::result::Result<(), String> {
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(format!("表达式在 {token:?} 之后还有多余内容")),
        }
    }

    fn parse_expr(&mut self) -> std::result::Result<Node, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("表达式嵌套过深（超过 {MAX_DEPTH} 层）"));
        }
        let node = self.parse_ternary();
        self.depth -= 1;
        node
    }

    fn parse_ternary(&mut self) -> std::result::Result<Node, String> {
        let cond = self.parse_nullish()?;
        if self.eat_punct("?") {
            let then = self.parse_expr()?;
            self.expect_punct(":")?;
            let els = self.parse_expr()?;
            return Ok(Node::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            });
        }
        Ok(cond)
    }

    fn parse_nullish(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_or()?;
        while self.eat_punct("??") {
            let right = self.parse_or()?;
            left = Node::Binary {
                op: BinOp::Coalesce,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_or(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_and()?;
        while self.eat_punct("||") {
            let right = self.parse_and()?;
            left = Node::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_equality()?;
        while self.eat_punct("&&") {
            let right = self.parse_equality()?;
            left = Node::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_relational()?;
        loop {
            let op = if self.eat_punct("===") {
                BinOp::StrictEq
            } else if self.eat_punct("!==") {
                BinOp::StrictNe
            } else if self.eat_punct("==") {
                BinOp::Eq
            } else if self.eat_punct("!=") {
                BinOp::Ne
            } else {
                return Ok(left);
            };
            let right = self.parse_relational()?;
            left = Node::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_relational(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_additive()?;
        loop {
            let op = if self.eat_punct("<=") {
                BinOp::Le
            } else if self.eat_punct(">=") {
                BinOp::Ge
            } else if self.eat_punct("<") {
                BinOp::Lt
            } else if self.eat_punct(">") {
                BinOp::Gt
            } else {
                return Ok(left);
            };
            let right = self.parse_additive()?;
            left = Node::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_additive(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = if self.eat_punct("+") {
                BinOp::Add
            } else if self.eat_punct("-") {
                BinOp::Sub
            } else {
                return Ok(left);
            };
            let right = self.parse_multiplicative()?;
            left = Node::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_multiplicative(&mut self) -> std::result::Result<Node, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.eat_punct("*") {
                BinOp::Mul
            } else if self.eat_punct("/") {
                BinOp::Div
            } else if self.eat_punct("%") {
                BinOp::Rem
            } else {
                return Ok(left);
            };
            let right = self.parse_unary()?;
            left = Node::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_unary(&mut self) -> std::result::Result<Node, String> {
        // 一元链不经过 `parse_expr`；不单独计数的话，10 万个 `!` 会直接打穿栈。
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("表达式嵌套过深（超过 {MAX_DEPTH} 层）"));
        }
        let result = self.parse_unary_inner();
        self.depth -= 1;
        result
    }

    fn parse_unary_inner(&mut self) -> std::result::Result<Node, String> {
        let op = if self.eat_punct("!") {
            Some(UnOp::Not)
        } else if self.eat_punct("-") {
            Some(UnOp::Neg)
        } else if self.eat_punct("+") {
            Some(UnOp::Pos)
        } else {
            None
        };
        match op {
            Some(op) => {
                let arg = self.parse_unary()?;
                Ok(Node::Unary {
                    op,
                    arg: Box::new(arg),
                })
            }
            None => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> std::result::Result<Node, String> {
        let mut base = self.parse_primary()?;
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > MAX_DEPTH {
                return Err(format!("成员访问链过长（超过 {MAX_DEPTH} 步）"));
            }
            if self.eat_punct(".") {
                let name = self.expect_ident("成员访问")?;
                base = Node::Member {
                    base: Box::new(base),
                    step: Step::Name {
                        name,
                        optional: false,
                    },
                };
            } else if self.eat_punct("?.") {
                if self.eat_punct("[") {
                    let index = self.parse_expr()?;
                    self.expect_punct("]")?;
                    base = Node::Member {
                        base: Box::new(base),
                        step: Step::Index {
                            index: Box::new(index),
                            optional: true,
                        },
                    };
                } else {
                    let name = self.expect_ident("`?.` 之后")?;
                    base = Node::Member {
                        base: Box::new(base),
                        step: Step::Name {
                            name,
                            optional: true,
                        },
                    };
                }
            } else if self.eat_punct("[") {
                let index = self.parse_expr()?;
                self.expect_punct("]")?;
                base = Node::Member {
                    base: Box::new(base),
                    step: Step::Index {
                        index: Box::new(index),
                        optional: false,
                    },
                };
            } else if self.eat_punct("(") {
                let mut args = Vec::new();
                if !self.eat_punct(")") {
                    loop {
                        args.push(self.parse_expr()?);
                        if self.eat_punct(",") {
                            continue;
                        }
                        self.expect_punct(")")?;
                        break;
                    }
                }
                base = Node::Call {
                    callee: Box::new(base),
                    args,
                };
            } else {
                return Ok(base);
            }
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<Node, String> {
        match self.peek() {
            Some(Token::Number(value)) => {
                let value = *value;
                self.pos += 1;
                Ok(Node::Lit(number_to_json(value)?))
            }
            Some(Token::Str(text)) => {
                let text = text.clone();
                self.pos += 1;
                Ok(Node::Lit(Config::String(text)))
            }
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.pos += 1;
                match name.as_str() {
                    "true" => Ok(Node::Lit(Config::Bool(true))),
                    "false" => Ok(Node::Lit(Config::Bool(false))),
                    // `null` 与 `undefined` 在配置里是同一个值（JSON 没有 `undefined`）。
                    "null" | "undefined" => Ok(Node::Lit(Config::Null)),
                    other => Ok(Node::Head(other.to_string())),
                }
            }
            Some(Token::Punct("(")) => {
                self.pos += 1;
                let inner = self.parse_expr()?;
                self.expect_punct(")")?;
                Ok(inner)
            }
            Some(Token::Punct("[")) => {
                Err("不支持数组字面量（子集里数组只能来自服务/`JSON.parse`）".into())
            }
            Some(Token::Punct("{")) => Err("不支持对象字面量".into()),
            Some(token) => Err(format!("这里不该出现 {token:?}")),
            None => Err("表达式不完整".into()),
        }
    }
}

// ---------------------------------------------------------------- 求值

/// 求值期的值。
#[derive(Debug, Clone)]
enum Value {
    Json(Config),
    /// 内建命名空间/函数族：`process`、`env`、`ctx`、`JSON`、`Number`、`String`、`Boolean`。
    Builtin(Builtin),
    /// **短路标记**：`a?.b.c` 里 `a` 为 nullish 时，整条链在 JS 里都不会再求值。
    /// 它像 `undefined` 一样为假、被 `??` 接住、参与字符串拼接时写作 `"undefined"`。
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Builtin {
    Process,
    Env,
    Ctx,
    Json,
    Number,
    String,
    Boolean,
}

fn eval(node: &Node, ctx: &Ctx) -> std::result::Result<Value, String> {
    match node {
        Node::Lit(value) => Ok(Value::Json(value.clone())),
        Node::Head(name) => match name.as_str() {
            "process" => Ok(Value::Builtin(Builtin::Process)),
            "env" => Ok(Value::Builtin(Builtin::Env)),
            "ctx" => Ok(Value::Builtin(Builtin::Ctx)),
            "JSON" => Ok(Value::Builtin(Builtin::Json)),
            "Number" => Ok(Value::Builtin(Builtin::Number)),
            "String" => Ok(Value::Builtin(Builtin::String)),
            "Boolean" => Ok(Value::Builtin(Builtin::Boolean)),
            other => Err(format!(
                "不支持裸标识符 `{other}`（内置子集只认 process/env/ctx/JSON/Number/String/Boolean；\
                 服务请写成 ctx.get('{other}')）"
            )),
        },
        Node::Member { base, step } => {
            let base = eval(base, ctx)?;
            eval_step(base, step, ctx)
        }
        Node::Call { callee, args } => eval_call(callee, args, ctx),
        Node::Unary { op, arg } => {
            let value = eval(arg, ctx)?;
            match op {
                UnOp::Not => Ok(Value::Json(Config::Bool(!truthy_value(&value)))),
                UnOp::Neg => match number_of(&value) {
                    Some(number) => Ok(Value::Json(number_to_json(-number)?)),
                    None => Err(format!("`-` 的操作数不能数字化：{}", describe(&value))),
                },
                UnOp::Pos => match number_of(&value) {
                    Some(number) => Ok(Value::Json(number_to_json(number)?)),
                    None => Err(format!("`+` 的操作数不能数字化：{}", describe(&value))),
                },
            }
        }
        Node::Binary { op, left, right } => eval_binary(*op, left, right, ctx),
        Node::Ternary { cond, then, els } => {
            let cond = eval(cond, ctx)?;
            if truthy_value(&cond) {
                eval(then, ctx)
            } else {
                eval(els, ctx)
            }
        }
    }
}

fn eval_step(base: Value, step: &Step, ctx: &Ctx) -> std::result::Result<Value, String> {
    // 短路之后整条链都不再求值（JS 的 `a?.b.c` 语义）：连下标表达式都不评。
    if matches!(base, Value::Missing) {
        return Ok(Value::Missing);
    }
    match step {
        Step::Name { name, optional } => {
            if *optional && is_nullish(&base) {
                return Ok(Value::Missing);
            }
            match base {
                Value::Json(Config::Null) => Err(format!("不能读 `null` 的属性 `{name}`（要容忍空值请写 `?.`）")),
                Value::Json(Config::Object(object)) => {
                    Ok(object.get(name).cloned().map(Value::Json).unwrap_or(Value::Missing))
                }
                Value::Json(Config::Array(list)) if name == "length" => {
                    Ok(Value::Json(Config::Number(Number::from(list.len() as u64))))
                }
                Value::Json(Config::String(text)) if name == "length" => {
                    // JS 数的是 UTF-16 码元；这里数字符（非 BMP 字符会有 1 vs 2 的差，附录 A）。
                    Ok(Value::Json(Config::Number(Number::from(text.chars().count() as u64))))
                }
                Value::Json(Config::Array(_)) => Err(format!(
                    "不支持数组方法 `{name}`（子集只有 `.length` 与下标；要 map/filter 请用宿主引擎）"
                )),
                Value::Json(Config::String(_)) => Err(format!(
                    "不支持字符串方法 `{name}`（子集只有 `.length`；要 split/startsWith 请用宿主引擎）"
                )),
                Value::Json(Config::Bool(_)) | Value::Json(Config::Number(_)) => Ok(Value::Missing),
                Value::Builtin(Builtin::Env) => Ok(std::env::var(name)
                    .ok()
                    .map(|text| Value::Json(Config::String(text)))
                    .unwrap_or(Value::Missing)),
                Value::Builtin(Builtin::Process) => match name.as_str() {
                    "platform" => Ok(Value::Json(Config::String(node_platform().to_string()))),
                    "version" => Ok(Value::Json(Config::String(format!("v{}", env!("CARGO_PKG_VERSION"))))),
                    "env" => Ok(Value::Builtin(Builtin::Env)),
                    "cwd" | "getBuiltinModule" => Err(format!(
                        "`process.{name}` 是函数，子集里它只能被调用（写 `process.{name}()`）"
                    )),
                    other => Err(format!("`process.{other}` 不在内置子集里")),
                },
                Value::Builtin(Builtin::Ctx) => service_value(ctx, name),
                Value::Builtin(other) => Err(format!("不能对 `{}` 取成员 `{name}`", builtin_name(other))),
                Value::Missing => unreachable!("上面已经短路"),
            }
        }
        Step::Index { index, optional } => {
            // `?.` 短路整条链：base 为 nullish 时**不能求值 index**
            //（`null?.[1/0]` 在 JS 里不会触发 Infinity 错误）。
            if *optional && is_nullish(&base) {
                return Ok(Value::Missing);
            }
            let index = eval(index, ctx)?;
            if matches!(index, Value::Missing) {
                return Ok(Value::Missing);
            }
            match base {
                Value::Json(Config::Null) => {
                    Err("不能给 `null` 取下标（要容忍空值请写 `?.[`）".into())
                }
                Value::Json(Config::Array(list)) => match integer_of(&index) {
                    Some(position) if position >= 0 && (position as usize) < list.len() => {
                        Ok(Value::Json(list[position as usize].clone()))
                    }
                    Some(_) => Ok(Value::Missing),
                    None => Err(format!("数组下标必须是整数：{}", describe(&index))),
                },
                Value::Json(Config::String(text)) => match integer_of(&index) {
                    Some(position) if position >= 0 => Ok(text
                        .chars()
                        .nth(position as usize)
                        .map(|c| Value::Json(Config::String(c.to_string())))
                        .unwrap_or(Value::Missing)),
                    _ => Err(format!("字符串下标必须是整数：{}", describe(&index))),
                },
                Value::Json(Config::Object(object)) => match as_string(&index) {
                    Some(key) => Ok(object
                        .get(&key)
                        .cloned()
                        .map(Value::Json)
                        .unwrap_or(Value::Missing)),
                    None => Err(format!("对象下标必须是字符串：{}", describe(&index))),
                },
                other => Err(format!("不能对 {} 取下标", describe(&other))),
            }
        }
    }
}

fn eval_call(callee: &Node, args: &[Node], ctx: &Ctx) -> std::result::Result<Value, String> {
    let path = static_path(callee).ok_or_else(|| dynamic_call_error(callee))?;
    let name = path.join(".");
    let expected = |count: usize| -> std::result::Result<(), String> {
        if args.len() == count {
            Ok(())
        } else {
            Err(format!(
                "`{name}(...)` 需要 {count} 个参数，收到 {}",
                args.len()
            ))
        }
    };
    let argument = |index: usize| -> std::result::Result<Value, String> {
        let node = args
            .get(index)
            .ok_or_else(|| format!("`{name}(...)` 缺少第 {} 个参数", index + 1))?;
        eval(node, ctx)
    };
    match name.as_str() {
        "Number" => {
            expected(1)?;
            let value = argument(0)?;
            match number_of(&value) {
                Some(number) => Ok(Value::Json(number_to_json(number)?)),
                None => Err(format!(
                    "`Number({})` 不能数字化：{}",
                    describe(&value),
                    describe(&value)
                )),
            }
        }
        "String" => {
            expected(1)?;
            let value = argument(0)?;
            match string_of(&value) {
                Some(text) => Ok(Value::Json(Config::String(text))),
                None => Err(format!(
                    "`String(...)` 不支持 {} （JS 的 Array/Object.toString 有自己的规则，子集不猜）",
                    describe(&value)
                )),
            }
        }
        "Boolean" => {
            expected(1)?;
            let value = argument(0)?;
            Ok(Value::Json(Config::Bool(truthy_value(&value))))
        }
        "JSON.parse" => {
            expected(1)?;
            let value = argument(0)?;
            let Some(text) = as_string(&value) else {
                return Err(format!(
                    "`JSON.parse(...)` 只接受字符串，收到 {}",
                    describe(&value)
                ));
            };
            let parsed: Config = serde_json::from_str(&text)
                .map_err(|err| format!("`JSON.parse(...)` 失败（参数必须是合法 JSON）: {err}"))?;
            Ok(Value::Json(parsed))
        }
        "process.cwd" => {
            expected(0)?;
            let dir =
                std::env::current_dir().map_err(|err| format!("`process.cwd()` 失败: {err}"))?;
            Ok(Value::Json(Config::String(
                dir.to_string_lossy().to_string(),
            )))
        }
        "ctx.get" => {
            expected(1)?;
            let value = argument(0)?;
            match as_string(&value) {
                Some(service) => service_value(ctx, &service),
                None => Err(format!(
                    "`ctx.get(...)` 只接受服务名（字符串），收到 {}",
                    describe(&value)
                )),
            }
        }
        other => Err(unsupported_call(other)),
    }
}

/// 子集外的调用：错误必须**点名它**，并给出下一步（换写法 / 注入引擎）。
fn unsupported_call(name: &str) -> String {
    let last = name.rsplit('.').next().unwrap_or(name);
    if name.contains("getBuiltinModule") {
        return format!(
            "不支持调用 `{name}`：内置子集不含 Node 内建模块。\
             这一形态（出厂 bundle 里真实存在）需要宿主注入 `js_evaluator`，或把配置改写成字面量"
        );
    }
    if name == "JSON.stringify" {
        return "不支持调用 `JSON.stringify`（内置子集不含它；把要比较的值直接写进表达式即可）"
            .into();
    }
    format!(
        "不支持调用 `{name}`（方法 `{last}` 不在内置子集里：只有 Number/String/Boolean/JSON.parse/\
         process.cwd/ctx.get；字符串与数组方法请用宿主引擎）"
    )
}

/// `ctx.get('<名>')` 与 `ctx.<名>` 的共同实现：读服务，按 JSON 收敛。
///
/// 三条口径（与 ABI 侧的"什么对插件可见"是同一件事）：
/// - `ExternValue` ⇒ 内层 JSON；
/// - 原生 `Config` 值 ⇒ 原样；
/// - 不存在的服务 ⇒ `null`（JS 里是 `undefined`，本仓两者都是 `null`）；
/// - **有但不可表达**（接口/裸 Rust 值）⇒ 报错：静默给 `null` 会让配置悄悄走错分支。
fn service_value(ctx: &Ctx, name: &str) -> std::result::Result<Value, String> {
    let Some(value) = ctx.get(name) else {
        return Ok(Value::Missing);
    };
    if let Some(exposed) = value.downcast::<ExternValue>() {
        return Ok(Value::Json(exposed.0.clone()));
    }
    if let Some(config) = value.downcast::<Config>() {
        return Ok(Value::Json((*config).clone()));
    }
    Err(format!(
        "服务 `{name}` 的值不是 JSON 可表达的（表达式里只读得到 ExternValue 与配置值；\
         带方法的服务请用真正的插件代码读）"
    ))
}

fn eval_binary(
    op: BinOp,
    left: &Node,
    right: &Node,
    ctx: &Ctx,
) -> std::result::Result<Value, String> {
    let left = match op {
        BinOp::And => {
            let value = eval(left, ctx)?;
            if !truthy_value(&value) {
                return Ok(value);
            }
            return eval(right, ctx);
        }
        BinOp::Or => {
            let value = eval(left, ctx)?;
            if truthy_value(&value) {
                return Ok(value);
            }
            return eval(right, ctx);
        }
        BinOp::Coalesce => {
            let value = eval(left, ctx)?;
            if !is_nullish(&value) {
                return Ok(value);
            }
            return eval(right, ctx);
        }
        _ => eval(left, ctx)?,
    };
    let right = eval(right, ctx)?;
    match op {
        BinOp::Add => add(&left, &right),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            let a = number_of(&left)
                .ok_or_else(|| format!("`{}` 的左操作数不能数字化", op_text(op)))?;
            let b = number_of(&right)
                .ok_or_else(|| format!("`{}` 的右操作数不能数字化", op_text(op)))?;
            let result = match op {
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => a % b, // JS 的 `%` 就是 C 的 fmod：符号跟被除数
                _ => unreachable!(),
            };
            number_to_json(result).map(Value::Json)
        }
        BinOp::StrictEq | BinOp::StrictNe => {
            no_builtin(&left, &right, op_text(op))?;
            let equal = strict_eq(&left, &right);
            Ok(Value::Json(Config::Bool(if op == BinOp::StrictEq {
                equal
            } else {
                !equal
            })))
        }
        BinOp::Eq | BinOp::Ne => {
            no_builtin(&left, &right, op_text(op))?;
            let equal = abstract_eq(&left, &right)?;
            Ok(Value::Json(Config::Bool(if op == BinOp::Eq {
                equal
            } else {
                !equal
            })))
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            no_builtin(&left, &right, op_text(op))?;
            let order = compare(&left, &right)?;
            let result = match (op, order) {
                (_, None) => false, // NaN 参与的比较恒为 false（JS 同）
                (BinOp::Lt, Some(order)) => order.is_lt(),
                (BinOp::Le, Some(order)) => order.is_le(),
                (BinOp::Gt, Some(order)) => order.is_gt(),
                (BinOp::Ge, Some(order)) => order.is_ge(),
                _ => unreachable!(),
            };
            Ok(Value::Json(Config::Bool(result)))
        }
        BinOp::And | BinOp::Or | BinOp::Coalesce => unreachable!("上面已短路"),
    }
}

/// 算术/比较的共同前提：内建命名空间不是数据。
fn no_builtin(left: &Value, right: &Value, op: &str) -> std::result::Result<(), String> {
    if matches!(left, Value::Builtin(_)) || matches!(right, Value::Builtin(_)) {
        return Err(format!(
            "`{op}` 不能作用于内建命名空间（{} / {}）",
            describe(left),
            describe(right)
        ));
    }
    Ok(())
}

fn add(left: &Value, right: &Value) -> std::result::Result<Value, String> {
    // `+` 的重载：任一侧是字符串 ⇒ 拼接；否则两侧数字化相加（JS 的 ToPrimitive 简化版）。
    let stringish = matches!(left, Value::Json(Config::String(_)))
        || matches!(right, Value::Json(Config::String(_)));
    if stringish {
        let a =
            string_of(left).ok_or_else(|| format!("`+` 不能把 {} 变成字符串", describe(left)))?;
        let b =
            string_of(right).ok_or_else(|| format!("`+` 不能把 {} 变成字符串", describe(right)))?;
        return Ok(Value::Json(Config::String(format!("{a}{b}"))));
    }
    let a =
        number_of(left).ok_or_else(|| format!("`+` 的左操作数不能数字化：{}", describe(left)))?;
    let b =
        number_of(right).ok_or_else(|| format!("`+` 的右操作数不能数字化：{}", describe(right)))?;
    number_to_json(a + b).map(Value::Json)
}

/// `===` / `!==`：类型与值都相等。null 与 undefined 视为同一个值（附录 A）。
fn strict_eq(left: &Value, right: &Value) -> bool {
    match (json_of(left), json_of(right)) {
        (Config::Null, Config::Null) => true,
        (Config::Bool(a), Config::Bool(b)) => a == b,
        (Config::Number(a), Config::Number(b)) => a.as_f64() == b.as_f64(),
        (Config::String(a), Config::String(b)) => a == b,
        // 对象/数组：本仓的值没有身份，结构比较是唯一有意义的口径（附录 A）。
        (Config::Array(a), Config::Array(b)) => a == b,
        (Config::Object(a), Config::Object(b)) => a == b,
        _ => false,
    }
}

/// `==` / `!=`：JS 的抽象相等（覆盖 JSON 能表达的原始值；对象/数组**拒绝**）。
fn abstract_eq(left: &Value, right: &Value) -> std::result::Result<bool, String> {
    let left = json_of(left);
    let right = json_of(right);
    if !is_primitive(&left) || !is_primitive(&right) {
        return Err("`==` 不能作用于数组/对象（JS 在这里比较身份，本仓的值没有身份；请用 === 或先取到标量）"
            .into());
    }
    match (&left, &right) {
        (Config::Null, Config::Null) => Ok(true),
        (Config::Null, _) | (_, Config::Null) => Ok(false), // JS 里 null 只与 undefined 相等
        (Config::String(a), Config::String(b)) => Ok(a == b), // 两侧都是字符串 ⇒ 比文本
        // 其余混合（Bool/Number/String）：JS 走数字化，NaN 参与时恒 false。
        _ => Ok(numeric(&left) == numeric(&right)),
    }
}

/// 大小比较：两侧都是字符串 ⇒ 比文本；否则按 JS 走数字化（`NaN` ⇒ `None` ⇒ 恒 false）。
fn compare(left: &Value, right: &Value) -> std::result::Result<Option<std::cmp::Ordering>, String> {
    let (a, b) = (json_of(left), json_of(right));
    if let (Config::String(a), Config::String(b)) = (&a, &b) {
        // JS 比的是 UTF-16 码元；这里是 Rust 的 char 序（ASCII 完全一致，附录 A）。
        return Ok(Some(a.cmp(b)));
    }
    if !is_primitive(&a) || !is_primitive(&b) {
        return Err(format!(
            "不支持 {} 与 {} 做大小比较（数组/对象的 `ToPrimitive` 规则不在子集里）",
            describe(left),
            describe(right)
        ));
    }
    Ok(numeric(&a).partial_cmp(&numeric(&b)))
}

// ---------------------------------------------------------------- 数值 / 字符串

fn is_primitive(value: &Config) -> bool {
    !matches!(value, Config::Array(_) | Config::Object(_))
}

fn is_nullish(value: &Value) -> bool {
    matches!(value, Value::Missing) || matches!(value, Value::Json(Config::Null))
}

/// 求值的**最终**收敛：短路标记 ⇒ `null`（JSON 里没有 `undefined`）；
/// 内建命名空间 ⇒ 报错（`!!js "process"` 不是配置值）。
fn finish(value: Value) -> std::result::Result<Config, String> {
    match value {
        Value::Json(value) => Ok(value),
        Value::Missing => Ok(Config::Null),
        Value::Builtin(builtin) => Err(format!(
            "表达式的结果是 `{}`（命名空间），不是配置值——要取它的成员或调用它的方法",
            builtin_name(builtin)
        )),
    }
}

/// 求值**中途**的收敛：调用点都已排除内建命名空间（[`no_builtin`]）。
fn json_of(value: &Value) -> Config {
    match value {
        Value::Json(value) => value.clone(),
        Value::Missing => Config::Null,
        Value::Builtin(builtin) => {
            debug_assert!(
                false,
                "内建命名空间 {} 不该走到取值",
                builtin_name(*builtin)
            );
            Config::Null
        }
    }
}

/// JS 的真值（`Value` 版）：短路标记为假，命名空间为真（JS 里对象恒为真）。
fn truthy_value(value: &Value) -> bool {
    match value {
        Value::Json(value) => truthy(value),
        Value::Missing => false,
        Value::Builtin(_) => true,
    }
}

/// JS 的 `ToNumber`（原始值）：`None` = **不可数字化**（数组/对象/内建），
/// `Some(NaN)` = 数字化的结果是 NaN（`Number('abc')`）。
fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Missing => Some(0.0), // `Number(undefined)` 在 JS 里是 NaN；本仓把 null/undefined 合一
        Value::Json(value) => match value {
            Config::Number(number) => number.as_f64(),
            Config::String(text) => number_of_text(text),
            Config::Bool(true) => Some(1.0),
            Config::Bool(false) => Some(0.0),
            Config::Null => Some(0.0),
            Config::Array(_) | Config::Object(_) => None,
        },
        Value::Builtin(_) => None,
    }
}

/// 按 JS 的 `ToNumber` 取数：原始值一定有结果（可数字化的给数，其余给 `NaN`）。
fn numeric(value: &Config) -> f64 {
    number_of(&Value::Json(value.clone())).unwrap_or(f64::NAN)
}

/// JS 的 `Number(文本)`：空串是 0，`0x` 前缀按十六进制，其余按十进制；
/// **不认识 `inf` / `NaN` 这类词**（JS 也不认识，`Number('inf')` 是 NaN）。
fn number_of_text(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    let (sign, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16)
            .ok()
            .map(|value| sign * value as f64);
    }
    let looks_numeric = body
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-');
    if !looks_numeric {
        return Some(f64::NAN);
    }
    body.parse::<f64>()
        .ok()
        .map(|value| sign * value)
        .or(Some(f64::NAN))
}

/// JS 的 `ToString`（原始值）：支持到字符串/数字/布尔/null/undefined，数组与对象**拒绝**。
fn string_of(value: &Value) -> Option<String> {
    match value {
        Value::Missing => Some("undefined".to_string()),
        Value::Builtin(_) => None,
        Value::Json(value) => match value {
            Config::String(text) => Some(text.clone()),
            Config::Number(number) => Some(number_to_text(number.as_f64().unwrap_or(f64::NAN))),
            Config::Bool(true) => Some("true".to_string()),
            Config::Bool(false) => Some("false".to_string()),
            Config::Null => Some("null".to_string()),
            Config::Array(_) | Config::Object(_) => None,
        },
    }
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::Json(Config::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::Json(Config::Number(number)) => number
            .as_f64()
            .filter(|number| number.fract() == 0.0)
            .map(|number| number as i64),
        _ => None,
    }
}

/// 数值 ⇒ 配置值：**能是整数就给整数**（`1+1` 该是 `2` 而不是 `2.0`），
/// `NaN`/`Infinity` 拒绝（JSON 里没有它们；`1/0` 在 JS 里是 `Infinity`）。
fn number_to_json(value: f64) -> std::result::Result<Config, String> {
    if !value.is_finite() {
        return Err(format!(
            "求值结果是 {}——它不是 JSON 值（JS 里的 NaN/Infinity 在这里一律报错，不降级成 null）",
            if value.is_nan() {
                "NaN".to_string()
            } else {
                format!("{value}")
            }
        ));
    }
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Ok(Config::Number(Number::from(value as i64)));
    }
    Number::from_f64(value)
        .map(Config::Number)
        .ok_or_else(|| format!("数值 {value} 无法表示成 JSON 值"))
}

fn number_to_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    // 只有 **2^53 以内的整数**可以安全地转 i64；1e20 这类值转 i64 会饱和，
    // 静默产出错误字符串。超出安全整数范围时交给 f64 的十进制格式化。
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// Node 的 `process.platform` 取值（与 `std::env::consts::OS` 只在少数几个平台上不同名）。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        "ios" => "darwin",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

// ---------------------------------------------------------------- 诊断文本

fn builtin_name(builtin: Builtin) -> &'static str {
    match builtin {
        Builtin::Process => "process",
        Builtin::Env => "env",
        Builtin::Ctx => "ctx",
        Builtin::Json => "JSON",
        Builtin::Number => "Number",
        Builtin::String => "String",
        Builtin::Boolean => "Boolean",
    }
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Missing | Value::Json(Config::Null) => "null/undefined",
        Value::Json(Config::Bool(_)) => "布尔值",
        Value::Json(Config::Number(_)) => "数字",
        Value::Json(Config::String(_)) => "字符串",
        Value::Json(Config::Array(_)) => "数组",
        Value::Json(Config::Object(_)) => "对象",
        Value::Builtin(builtin) => builtin_name(*builtin),
    }
}

fn describe_node(node: &Node) -> &'static str {
    match node {
        Node::Lit(_) => "字面量",
        Node::Head(_) => "标识符",
        Node::Member { .. } => "成员访问",
        Node::Call { .. } => "调用结果",
        Node::Unary { .. } | Node::Binary { .. } | Node::Ternary { .. } => "运算符表达式",
    }
}

fn op_text(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::StrictEq => "===",
        BinOp::StrictNe => "!==",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Coalesce => "??",
    }
}

/// 一条**全静态**的路径（`process.cwd`、`ctx.get`、`Number` …）——只有它能被调用。
fn static_path(node: &Node) -> Option<Vec<String>> {
    match node {
        Node::Head(name) => Some(vec![name.clone()]),
        Node::Member { base, step } => {
            let mut path = static_path(base)?;
            match step {
                Step::Name {
                    name,
                    optional: false,
                } => {
                    path.push(name.clone());
                    Some(path)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// 从一条成员链里剥出"尾部的名字"与"链的底"：
/// `a.b.c` ⇒ (`["b","c"]`, `a`)；`f(x).c` ⇒ (`["c"]`, `f(x)`）。
fn peel(node: &Node) -> (Vec<String>, &Node) {
    let mut trailing = Vec::new();
    let mut cursor = node;
    while let Node::Member { base, step } = cursor {
        match step {
            Step::Name {
                name,
                optional: false,
            } => trailing.push(name.clone()),
            _ => break,
        }
        cursor = base;
    }
    trailing.reverse();
    (trailing, cursor)
}

/// **动态调用**（`process.getBuiltinModule('node:path').resolve('x')` 这种）的报错。
///
/// 子集只认具名调用，所以这类形态一律拒绝——但错误要尽量说清"是哪一段越界了"：
/// 出厂 bundle 里最常见的正是"某个静态名字的**返回值**上再调方法"。
fn dynamic_call_error(callee: &Node) -> String {
    let (trailing, base) = peel(callee);
    let last = trailing.last().cloned().unwrap_or_else(|| "?".to_string());
    match base {
        Node::Head(head) => {
            let name = std::iter::once(head.clone())
                .chain(trailing.iter().cloned())
                .collect::<Vec<_>>()
                .join(".");
            unsupported_call(&name)
        }
        Node::Call { callee: inner, .. } => {
            let inner = static_path(inner)
                .map(|path| path.join("."))
                .unwrap_or_else(|| describe_node(inner).to_string());
            format!(
                "不支持调用 `{inner}(...)` 的返回值上的方法 `{last}`\
                 （内置子集只支持具名调用：Number/String/Boolean/JSON.parse/process.cwd/ctx.get）"
            )
        }
        other => format!(
            "不支持调用 {} 上的 `{last}`（内置子集只支持具名调用：\
             Number/String/Boolean/JSON.parse/process.cwd/ctx.get）",
            describe_node(other)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_core::App;

    /// 内置子集的**纯函数**面（表驱动在 `tests/interpolation.rs`；这里只钉两条最要紧的
    /// 边界：真值语义与"不可表达的值不降级"）。
    #[test]
    fn truthiness_matches_javascript() {
        assert!(!truthy(&Config::Null));
        assert!(!truthy(&Config::Bool(false)));
        assert!(!truthy(&Config::Number(Number::from(0))));
        assert!(!truthy(&Config::String(String::new())));
        assert!(truthy(&Config::Array(vec![])));
        assert!(truthy(&Config::Object(Default::default())));
    }

    #[test]
    fn non_json_values_are_rejected_not_nulled() {
        let app = App::new();
        let js = BuiltinJs;
        let err = js
            .evaluate("1 / 0", &app.root)
            .expect_err("Infinity 不是 JSON 值");
        assert!(err.contains("JSON"), "{err}");
        let err = js
            .evaluate("({}).a", &app.root)
            .expect_err("对象字面量不在子集里");
        assert!(err.contains("对象字面量"), "{err}");
    }
}
