//! SQL 转储文件（mysqldump / pg_dump）的流式语句拆分与 CREATE TABLE 解析。
//! 逐行读取，状态机感知字符串 / 行注释 / 块注释 / DELIMITER 指令，分隔符只在这些
//! 上下文之外生效。注释文本原样保留在语句里（MySQL 的版本化注释 /*!...*/ 需要执行）。

use std::io::BufRead;

/// 同时限制单行读取和单条语句；不会先读入任意大的行再检查。
pub const MAX_IMPORT_STATEMENT_BYTES: usize = 16 * 1024 * 1024;
/// 客户端解析指令的资源预算，和服务端 SQL/业务规则无关。
const MAX_IMPORT_DELIMITER_BYTES: usize = 128;

fn statement_limit() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "IMPORT_STATEMENT_LIMIT: 单行或单条 SQL 语句超过导入字节预算",
    )
}

fn append_statement(out: &mut String, text: &str, maximum: usize) -> std::io::Result<()> {
    if text.len() > maximum.saturating_sub(out.len()) {
        return Err(statement_limit());
    }
    out.push_str(text);
    Ok(())
}

fn bounded_line(reader: &mut impl BufRead, maximum: usize) -> std::io::Result<Option<String>> {
    let mut line = Vec::with_capacity(maximum.min(8192));
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        // 在拷贝之前拒绝超预算数据，reader 的缓冲区也不会被要求无界增长。
        if length > maximum.saturating_sub(line.len()) {
            return Err(statement_limit());
        }
        let ended = buffer[length - 1] == b'\n';
        line.extend_from_slice(&buffer[..length]);
        reader.consume(length);
        if ended {
            break;
        }
    }
    if line.is_empty() {
        return Ok(None);
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub struct SqlSplitter {
    delimiter: String,
    /// MySQL 方言：反斜杠是转义符、`#` 是行注释；PG 都不是
    mysql_mode: bool,
    /// 上一条语句分隔符之后遗留的未扫描文本
    pending: String,
    /// 当前引号的关闭字符；方括号标识符使用 b']'。
    in_str: Option<u8>, // b'\'' | b'"' | b'`' | b']'
    in_block_comment: bool,
    escape_next: bool,
    max_statement_bytes: usize,
}

impl SqlSplitter {
    pub fn new(mysql_mode: bool) -> Self {
        Self::with_max_statement_bytes(mysql_mode, MAX_IMPORT_STATEMENT_BYTES)
    }

    /// 允许测试在很小预算下验证失败前不会发送半条 SQL。
    pub fn with_max_statement_bytes(mysql_mode: bool, max_statement_bytes: usize) -> Self {
        Self {
            delimiter: ";".to_string(),
            mysql_mode,
            pending: String::new(),
            in_str: None,
            in_block_comment: false,
            escape_next: false,
            max_statement_bytes,
        }
    }

    /// 读取下一条完整语句（已 trim）；EOF 且无残留时返回 None
    pub fn next_statement(&mut self, reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
        let mut out = String::new();
        loop {
            let from_pending = !self.pending.is_empty();
            let line = if from_pending {
                std::mem::take(&mut self.pending)
            } else {
                let Some(line) = bounded_line(reader, self.max_statement_bytes)? else {
                    if self.in_str.is_some() || self.in_block_comment || self.escape_next {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "IMPORT_PARSE_ERROR: 文件末尾存在未闭合字符串、标识符或块注释",
                        ));
                    }
                    let t = out.trim().to_string();
                    return Ok(if t.is_empty() { None } else { Some(t) });
                };
                line
            };

            // DELIMITER 指令：只在语句缓冲为空、且是文件中独立行时生效
            if !from_pending {
                let t = line.trim_start();
                let tb = t.as_bytes();
                if tb.len() >= 9
                    && tb[..9].eq_ignore_ascii_case(b"delimiter")
                    && (tb.len() == 9 || tb[9].is_ascii_whitespace())
                    && out.trim().is_empty()
                {
                    // 前 9 字节已确认是 ASCII "delimiter"，此处切片安全
                    let d = t[9..].trim();
                    if d.is_empty() || d.len() > MAX_IMPORT_DELIMITER_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "IMPORT_PARSE_ERROR: DELIMITER 必须为 1 至 128 字节",
                        ));
                    }
                    self.delimiter = d.to_string();
                    continue;
                }
            }

            let bytes = line.as_bytes();
            let delim = self.delimiter.as_bytes();
            let mut i = 0usize;
            while i < bytes.len() {
                let c = bytes[i];

                // 正常状态先匹配完整分隔符，也支持 UTF-8 自定义分隔符。
                if self.in_str.is_none()
                    && !self.in_block_comment
                    && !self.escape_next
                    && bytes[i..].starts_with(delim)
                {
                    let stmt = out.trim().to_string();
                    self.pending = line[i + delim.len()..].to_string();
                    if stmt.is_empty() {
                        // 空语句（连续分隔符）：直接继续扫 pending。
                        out.clear();
                        break;
                    }
                    return Ok(Some(stmt));
                }

                // 多字节 UTF-8 字符：整字符原样保留（不可能命中任何 ASCII 语法记号）
                if !c.is_ascii() {
                    let ch = line[i..].chars().next().unwrap();
                    append_statement(
                        &mut out,
                        &line[i..i + ch.len_utf8()],
                        self.max_statement_bytes,
                    )?;
                    // MySQL 反斜杠只转义紧随的一个字符，不能泄漏到后续引号。
                    self.escape_next = false;
                    i += ch.len_utf8();
                    continue;
                }

                if self.escape_next {
                    append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                    self.escape_next = false;
                    i += 1;
                    continue;
                }

                if let Some(q) = self.in_str {
                    append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                    if self.mysql_mode && q != b']' && c == b'\\' {
                        self.escape_next = true;
                    } else if c == q {
                        // 双写引号 = 转义（SQL 标准）
                        if i + 1 < bytes.len() && bytes[i + 1] == q {
                            append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                            i += 1;
                        } else {
                            self.in_str = None;
                        }
                    }
                    i += 1;
                    continue;
                }

                if self.in_block_comment {
                    append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                    if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        append_statement(&mut out, "/", self.max_statement_bytes)?;
                        i += 2;
                        self.in_block_comment = false;
                    } else {
                        i += 1;
                    }
                    continue;
                }

                match c {
                    b'\'' | b'"' | b'`' => {
                        self.in_str = Some(c);
                        append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                        i += 1;
                    }
                    b'[' => {
                        self.in_str = Some(b']');
                        append_statement(&mut out, "[", self.max_statement_bytes)?;
                        i += 1;
                    }
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                        self.in_block_comment = true;
                        append_statement(&mut out, "/*", self.max_statement_bytes)?;
                        i += 2;
                    }
                    b'-' if i + 1 < bytes.len()
                        && bytes[i + 1] == b'-'
                        && (i + 2 >= bytes.len() || bytes[i + 2].is_ascii_whitespace()) =>
                    {
                        // 行注释：整行剩余部分原样保留
                        append_statement(&mut out, &line[i..], self.max_statement_bytes)?;
                        i = bytes.len();
                    }
                    b'#' if self.mysql_mode => {
                        append_statement(&mut out, &line[i..], self.max_statement_bytes)?;
                        i = bytes.len();
                    }
                    _ => {
                        append_statement(&mut out, &line[i..i + 1], self.max_statement_bytes)?;
                        i += 1;
                    }
                }
            }
        }
    }
}

/// 跳过空白与注释（-- 、#、/* */），返回新位置；EOF 返回 None
fn skip_ws_comments(sql: &str, mut i: usize) -> Option<usize> {
    let b = sql.as_bytes();
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < b.len()
            && b[i] == b'-'
            && b[i + 1] == b'-'
            && (i + 2 >= b.len() || b[i + 2].is_ascii_whitespace())
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i < b.len() && b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        break;
    }
    if i >= b.len() {
        None
    } else {
        Some(i)
    }
}

/// 读取一个单词（字母/数字/下划线），返回 (单词, 结束位置)
fn read_word(b: &[u8], i: usize) -> Option<(String, usize)> {
    let start = i;
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if j == start {
        None
    } else {
        Some((String::from_utf8_lossy(&b[start..j]).to_string(), j))
    }
}

/// 扫描一个真实标识符段；引号内的点、空格、分号和注释记号都只是名称字符。
fn read_identifier(sql: &str, start: usize) -> Option<(usize, String)> {
    let bytes = sql.as_bytes();
    let first = *bytes.get(start)?;
    if matches!(first, b'`' | b'"' | b'[') {
        let close = if first == b'[' { b']' } else { first };
        let mut index = start + 1;
        while index < bytes.len() {
            if bytes[index] == close {
                if bytes.get(index + 1) == Some(&close) {
                    index += 2;
                    continue;
                }
                let doubled = format!("{}{}", close as char, close as char);
                let decoded = sql[start + 1..index].replace(&doubled, &(close as char).to_string());
                return Some((index + 1, decoded));
            }
            index += 1;
        }
        return None;
    }
    let mut chars = sql[start..].char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }
    let mut end = start + first.len_utf8();
    for (offset, character) in chars {
        if !(character.is_alphanumeric() || character == '_' || character == '$') {
            break;
        }
        end = start + offset + character.len_utf8();
    }
    Some((end, sql[start..end].to_owned()))
}

/// 返回限定名称的原文结束位置及解码后的末段；不按引号内的点切分。
fn read_qualified_identifier(sql: &str, start: usize) -> Option<(usize, String)> {
    let (mut end, mut last) = read_identifier(sql, start)?;
    loop {
        let Some(next) = skip_ws_comments(sql, end) else {
            break;
        };
        if sql.as_bytes()[next] != b'.' {
            break;
        }
        let segment = skip_ws_comments(sql, next + 1)?;
        (end, last) = read_identifier(sql, segment)?;
    }
    Some((end, last))
}

/// 识别 CREATE TABLE 语句。
/// 返回 (表名原文 token, "TABLE" 关键字结束位置, 是否已带 IF NOT EXISTS)
fn create_table_details(sql: &str) -> Option<(std::ops::Range<usize>, usize, bool)> {
    let b = sql.as_bytes();
    let mut i = skip_ws_comments(sql, 0)?;
    let (kw, next) = read_word(b, i)?;
    if !kw.eq_ignore_ascii_case("create") {
        return None;
    }
    i = skip_ws_comments(sql, next)?;
    let (kw2, after_table) = read_word(b, i)?;
    if !kw2.eq_ignore_ascii_case("table") {
        return None;
    }
    i = skip_ws_comments(sql, after_table)?;

    // 可选 IF NOT EXISTS
    let mut has_ine = false;
    if let Some((w, n)) = read_word(b, i) {
        if w.eq_ignore_ascii_case("if") {
            if let Some(j) = skip_ws_comments(sql, n) {
                if let Some((w2, n2)) = read_word(b, j) {
                    if w2.eq_ignore_ascii_case("not") {
                        if let Some(j2) = skip_ws_comments(sql, n2) {
                            if let Some((w3, n3)) = read_word(b, j2) {
                                if w3.eq_ignore_ascii_case("exists") {
                                    has_ine = true;
                                    i = skip_ws_comments(sql, n3)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 只对明确识别的限定名称执行 skip/overwrite，未知或残缺语法不猜目标。
    let start = i;
    let (end, _) = read_qualified_identifier(sql, start)?;
    let definition = skip_ws_comments(sql, end)?;
    if b[definition] != b'(' {
        let (keyword, _) = read_word(b, definition)?;
        if !keyword.eq_ignore_ascii_case("as") && !keyword.eq_ignore_ascii_case("like") {
            return None;
        }
    }
    Some((start..end, after_table, has_ine))
}

/// 返回 CREATE TABLE 的目标名称原文范围，保留其余 DDL 字节不变。
/// TEMP/VIRTUAL 等未识别变体返回 None，由调用方明确拒绝需要改写的操作。
pub fn create_table_target_span(sql: &str) -> Option<std::ops::Range<usize>> {
    create_table_details(sql).map(|(span, _, _)| span)
}

/// 兼容原签名；名称解析和 span 改写共用同一个词法扫描结果。
pub fn parse_create_table(sql: &str) -> Option<(String, usize, bool)> {
    let (span, after_table, has_ine) = create_table_details(sql)?;
    Some((sql[span].to_owned(), after_table, has_ine))
}

/// 解码完整的逐段标识符，引用内的点号不会被拆开。
pub fn decode_identifier_parts(token: &str) -> Option<Vec<String>> {
    let mut start = skip_ws_comments(token, 0)?;
    let mut parts = Vec::new();
    loop {
        let (end, decoded) = read_identifier(token, start)?;
        if decoded.is_empty() {
            return None;
        }
        parts.push(decoded);
        let Some(next) = skip_ws_comments(token, end) else {
            return Some(parts);
        };
        if token.as_bytes()[next] != b'.' {
            return None;
        }
        start = skip_ws_comments(token, next + 1)?;
    }
}

/// 表名 token 归一化：取最后一段、去引号、小写（用于存在性比较）
pub fn normalize_table_name(token: &str) -> String {
    let parsed = skip_ws_comments(token, 0)
        .and_then(|start| read_qualified_identifier(token, start))
        .filter(|(end, _)| token[*end..].trim().is_empty());
    parsed
        .map(|(_, last)| last.to_lowercase())
        .unwrap_or_default()
}

// ---------- 转储（导出）辅助 ----------

/// JSON 值转 SQL 字面量。mysql_mode 下转义反斜杠并用 \'，PG 用标准 '' 双写。
pub fn sql_literal(v: &Option<serde_json::Value>, mysql_mode: bool) -> String {
    use serde_json::Value as J;
    match v {
        None | Some(J::Null) => "NULL".to_string(),
        Some(J::Number(n)) => n.to_string(),
        Some(J::Bool(b)) => if *b { "1" } else { "0" }.to_string(),
        Some(J::String(s)) => quote_string(s, mysql_mode),
        Some(other) => quote_string(&other.to_string(), mysql_mode),
    }
}

fn quote_string(s: &str, mysql_mode: bool) -> String {
    if mysql_mode {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// 追加批量 INSERT 语句（每 100 行一条多值 INSERT），返回写入行数
pub fn write_insert_batches(
    out: &mut String,
    quoted_table: &str,
    quoted_columns: &str,
    rows: &[Vec<Option<serde_json::Value>>],
    mysql_mode: bool,
) -> u64 {
    const BATCH: usize = 100;
    for chunk in rows.chunks(BATCH) {
        out.push_str(&format!(
            "INSERT INTO {} ({}) VALUES\n",
            quoted_table, quoted_columns
        ));
        for (i, row) in chunk.iter().enumerate() {
            let vals: Vec<String> = row.iter().map(|v| sql_literal(v, mysql_mode)).collect();
            out.push('(');
            out.push_str(&vals.join(", "));
            out.push(')');
            out.push_str(if i + 1 < chunk.len() { ",\n" } else { ";\n\n" });
        }
    }
    rows.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn split_all(sql: &str, mysql: bool) -> Vec<String> {
        let mut sp = SqlSplitter::new(mysql);
        let mut r = BufReader::new(sql.as_bytes());
        let mut out = Vec::new();
        while let Some(s) = sp.next_statement(&mut r).unwrap() {
            out.push(s);
        }
        out
    }

    #[test]
    fn single_statement() {
        assert_eq!(split_all("SELECT 1;", true), vec!["SELECT 1"]);
    }

    #[test]
    fn basic_multi_statement() {
        let v = split_all(
            "CREATE TABLE a (id INT);\nINSERT INTO a VALUES (1);\n",
            true,
        );
        assert_eq!(
            v,
            vec!["CREATE TABLE a (id INT)", "INSERT INTO a VALUES (1)"]
        );
    }

    #[test]
    fn multiline_statement() {
        assert_eq!(
            split_all(
                "CREATE TABLE a (\n  id INT,\n  name TEXT\n);\nSELECT\n  1;",
                true
            ),
            vec!["CREATE TABLE a (\n  id INT,\n  name TEXT\n)", "SELECT\n  1"]
        );
    }

    #[test]
    fn semicolon_inside_string() {
        let v = split_all("INSERT INTO a VALUES ('x;y', \"p;q\");\nSELECT 1;", true);
        assert_eq!(v, vec!["INSERT INTO a VALUES ('x;y', \"p;q\")", "SELECT 1"]);
    }

    #[test]
    fn doubled_quotes_and_quoted_identifier() {
        let sql = "SELECT 'a'';b', \"c\"\";d\", `e``;f`; SELECT 2;";
        assert_eq!(
            split_all(sql, true),
            vec!["SELECT 'a'';b', \"c\"\";d\", `e``;f`", "SELECT 2"]
        );
    }

    #[test]
    fn multiline_string_and_block_comment() {
        let sql = "SELECT '第一行;\n第二行';\n/* 注释;\n仍是注释; */ SELECT 2;";
        assert_eq!(
            split_all(sql, true),
            vec![
                "SELECT '第一行;\n第二行'",
                "/* 注释;\n仍是注释; */ SELECT 2"
            ]
        );
    }

    #[test]
    fn backslash_escape_mysql_not_pg() {
        // MySQL: \' 不结束字符串
        let v = split_all("INSERT INTO a VALUES ('a\\'b');\nSELECT 1;", true);
        assert_eq!(v, vec!["INSERT INTO a VALUES ('a\\'b')", "SELECT 1"]);
        // PG: 反斜杠不是转义，'a\' 就结束字符串（\\' 后引号关闭）
        let v2 = split_all("INSERT INTO a VALUES ('a\\');\nSELECT 1;", false);
        assert_eq!(v2, vec!["INSERT INTO a VALUES ('a\\')", "SELECT 1"]);
    }

    #[test]
    fn comments_and_versioned() {
        let v = split_all(
            "-- comment;\n/*!40101 SET NAMES utf8mb4 */;\n# sharp;\nSELECT 1; -- trailing\n",
            true,
        );
        // 语句1 = 行注释+版本化注释；语句2 = # 注释+SELECT 1；语句3 = 末尾残留注释
        assert_eq!(
            v,
            vec![
                "-- comment;\n/*!40101 SET NAMES utf8mb4 */",
                "# sharp;\nSELECT 1",
                "-- trailing"
            ]
        );
    }

    #[test]
    fn delimiter_directive() {
        let sql = "DELIMITER ;;\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END;;\nDELIMITER ;\nSELECT 3;";
        let v = split_all(sql, true);
        assert_eq!(
            v,
            vec![
                "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                "SELECT 3"
            ]
        );
    }

    #[test]
    fn multiple_statements_one_line() {
        let v = split_all(";; SELECT 1; SELECT 2;  ;; SELECT 3;;;", true);
        assert_eq!(v, vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn unicode_content() {
        let v = split_all(
            "INSERT INTO t VALUES ('中文;内容');\nSELECT '日本語';",
            true,
        );
        assert_eq!(
            v,
            vec!["INSERT INTO t VALUES ('中文;内容')", "SELECT '日本語'"]
        );
    }

    #[test]
    fn final_statement_without_delimiter() {
        assert_eq!(
            split_all("SELECT 1;\n SELECT '末尾无分号'  \n", true),
            vec!["SELECT 1", "SELECT '末尾无分号'"]
        );
        assert_eq!(split_all("SELECT\n  1", false), vec!["SELECT\n  1"]);
    }

    #[test]
    fn empty_input_and_empty_statements() {
        assert!(split_all("", true).is_empty());
        assert!(split_all("  \n \t", true).is_empty());
        assert!(split_all(";; ;\n;", true).is_empty());
    }

    #[test]
    fn parse_create_table_variants() {
        let (name, _, ine) = parse_create_table("CREATE TABLE `my_db`.`users` (id INT);").unwrap();
        assert_eq!(name, "`my_db`.`users`");
        assert!(!ine);

        let (name, _, ine) =
            parse_create_table("-- c\nCREATE TABLE IF NOT EXISTS \"public\".\"t\" (id int);")
                .unwrap();
        assert_eq!(name, "\"public\".\"t\"");
        assert!(ine);

        assert!(parse_create_table("CREATE INDEX idx ON t (c);").is_none());
        assert!(parse_create_table("INSERT INTO t VALUES (1);").is_none());
    }

    #[test]
    fn normalize_names() {
        assert_eq!(normalize_table_name("`my_db`.`Users`"), "users");
        assert_eq!(normalize_table_name("\"public\".\"T\""), "t");
        assert_eq!(normalize_table_name("plain_name"), "plain_name");
    }
}

#[cfg(test)]
mod identity_span_tests {
    use super::*;

    #[test]
    fn create_target_span_preserves_unrelated_quoted_text_and_complex_names() {
        let sql = "CREATE TABLE IF NOT EXISTS /* 目标前注释 */ \"literal .\"\" table\" (\"literal .\"\" table\" TEXT DEFAULT 'literal .\" table')";
        let span = create_table_target_span(sql).unwrap();
        assert_eq!(&sql[span.clone()], "\"literal .\"\" table\"");
        let qualified = "\"main\".\"literal .\"\" table\"";
        let rewritten = format!("{}{}{}", &sql[..span.start], qualified, &sql[span.end..]);
        assert!(rewritten.ends_with("(\"literal .\"\" table\" TEXT DEFAULT 'literal .\" table')"));
        assert!(rewritten.starts_with("CREATE TABLE IF NOT EXISTS /* 目标前注释 */ \"main\"."));
        assert!(
            create_table_target_span("CREATE VIRTUAL TABLE item USING fts5(content)").is_none()
        );
    }

    #[test]
    fn decode_identifier_parts_keeps_dots_spaces_and_escaped_quotes_inside_each_segment() {
        assert_eq!(
            decode_identifier_parts("\"schema .\"\" name\".\"table .\"\" name\""),
            Some(vec!["schema .\" name".into(), "table .\" name".into()])
        );
        assert_eq!(
            decode_identifier_parts("`db``. name`. /*段间注释*/ `table``. name`"),
            Some(vec!["db`. name".into(), "table`. name".into()])
        );
        assert_eq!(
            decode_identifier_parts("[schema]]. name].[table]]. name]"),
            Some(vec!["schema]. name".into(), "table]. name".into()])
        );
        assert!(decode_identifier_parts("\"partial\".").is_none());
        assert!(decode_identifier_parts("\"literal\" extra").is_none());
    }
}
