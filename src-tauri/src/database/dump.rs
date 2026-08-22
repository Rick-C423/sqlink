//! SQL 转储文件（mysqldump / pg_dump）的流式语句拆分与 CREATE TABLE 解析。
//! 逐行读取，状态机感知字符串 / 行注释 / 块注释 / DELIMITER 指令，分隔符只在这些
//! 上下文之外生效。注释文本原样保留在语句里（MySQL 的版本化注释 /*!...*/ 需要执行）。

use std::io::BufRead;

pub struct SqlSplitter {
    delimiter: String,
    /// MySQL 方言：反斜杠是转义符、`#` 是行注释；PG 都不是
    mysql_mode: bool,
    /// 上一条语句分隔符之后遗留的未扫描文本
    pending: String,
    in_str: Option<u8>, // b'\'' | b'"' | b'`'
    in_block_comment: bool,
    escape_next: bool,
}

impl SqlSplitter {
    pub fn new(mysql_mode: bool) -> Self {
        Self {
            delimiter: ";".to_string(),
            mysql_mode,
            pending: String::new(),
            in_str: None,
            in_block_comment: false,
            escape_next: false,
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
                let mut l = String::new();
                if reader.read_line(&mut l)? == 0 {
                    let t = out.trim().to_string();
                    return Ok(if t.is_empty() { None } else { Some(t) });
                }
                l
            };

            // DELIMITER 指令：只在语句缓冲为空、且是文件中独立行时生效
            if !from_pending && out.trim().is_empty() {
                let t = line.trim_start();
                let tb = t.as_bytes();
                if tb.len() > 9 && tb[..9].eq_ignore_ascii_case(b"delimiter") {
                    // 前 9 字节已确认是 ASCII "delimiter"，此处切片安全
                    let d = t[9..].trim();
                    if !d.is_empty() {
                        self.delimiter = d.to_string();
                    }
                    continue;
                }
            }

            let bytes = line.as_bytes();
            let delim = self.delimiter.as_bytes();
            let mut i = 0usize;
            while i < bytes.len() {
                let c = bytes[i];

                // 多字节 UTF-8 字符：整字符原样保留（不可能命中任何 ASCII 语法记号）
                if !c.is_ascii() {
                    let ch = line[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                    continue;
                }

                if self.escape_next {
                    out.push(c as char);
                    self.escape_next = false;
                    i += 1;
                    continue;
                }

                if let Some(q) = self.in_str {
                    out.push(c as char);
                    if self.mysql_mode && c == b'\\' {
                        self.escape_next = true;
                    } else if c == q {
                        // 双写引号 = 转义（SQL 标准）
                        if i + 1 < bytes.len() && bytes[i + 1] == q {
                            out.push(q as char);
                            i += 1;
                        } else {
                            self.in_str = None;
                        }
                    }
                    i += 1;
                    continue;
                }

                if self.in_block_comment {
                    out.push(c as char);
                    if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        out.push('/');
                        i += 2;
                        self.in_block_comment = false;
                    } else {
                        i += 1;
                    }
                    continue;
                }

                // 正常状态：先匹配分隔符
                if bytes[i..].starts_with(delim) {
                    out.push_str(&line[..i]);
                    let stmt = out.trim().to_string();
                    self.pending = line[i + delim.len()..].to_string();
                    if stmt.is_empty() {
                        // 空语句（连续分隔符）：直接继续扫 pending
                        out.clear();
                        break;
                    }
                    return Ok(Some(stmt));
                }

                match c {
                    b'\'' | b'"' | b'`' => {
                        self.in_str = Some(c);
                        out.push(c as char);
                        i += 1;
                    }
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                        self.in_block_comment = true;
                        out.push('/');
                        out.push('*');
                        i += 2;
                    }
                    b'-' if i + 1 < bytes.len()
                        && bytes[i + 1] == b'-'
                        && (i + 2 >= bytes.len() || bytes[i + 2].is_ascii_whitespace()) =>
                    {
                        // 行注释：整行剩余部分原样保留
                        out.push_str(&line[i..]);
                        i = bytes.len();
                    }
                    b'#' if self.mysql_mode => {
                        out.push_str(&line[i..]);
                        i = bytes.len();
                    }
                    _ => {
                        out.push(c as char);
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
        if i + 1 < b.len() && b[i] == b'-' && b[i + 1] == b'-'
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

/// 识别 CREATE TABLE 语句。
/// 返回 (表名原文 token, "TABLE" 关键字结束位置, 是否已带 IF NOT EXISTS)
pub fn parse_create_table(sql: &str) -> Option<(String, usize, bool)> {
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

    // 表名 token：`db`.`t` / "db"."t" / db.t / t
    let start = i;
    while i < b.len() {
        let c = b[i];
        if c == b'`' || c == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if i + 1 < b.len() && b[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else if c.is_ascii_whitespace() || c == b'(' {
            break;
        } else {
            i += 1;
        }
    }
    if start == i {
        None
    } else {
        Some((sql[start..i].to_string(), after_table, has_ine))
    }
}

/// 表名 token 归一化：取最后一段、去引号、小写（用于存在性比较）
pub fn normalize_table_name(token: &str) -> String {
    let last = token.rsplit('.').next().unwrap_or(token).trim();
    let unquoted = if (last.starts_with('`') && last.ends_with('`') && last.len() >= 2)
        || (last.starts_with('"') && last.ends_with('"') && last.len() >= 2)
    {
        &last[1..last.len() - 1]
    } else {
        last
    };
    unquoted.replace("``", "`").replace("\"\"", "\"").to_lowercase()
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
        out.push_str(&format!("INSERT INTO {} ({}) VALUES\n", quoted_table, quoted_columns));
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
    fn basic_multi_statement() {
        let v = split_all("CREATE TABLE a (id INT);\nINSERT INTO a VALUES (1);\n", true);
        assert_eq!(v.len(), 2);
        assert!(v[0].starts_with("CREATE TABLE a"));
        assert!(v[1].starts_with("INSERT INTO a"));
    }

    #[test]
    fn semicolon_inside_string() {
        let v = split_all("INSERT INTO a VALUES ('x;y', \"p;q\");\nSELECT 1;", true);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("'x;y'"));
    }

    #[test]
    fn backslash_escape_mysql_not_pg() {
        // MySQL: \' 不结束字符串
        let v = split_all("INSERT INTO a VALUES ('a\\'b');\nSELECT 1;", true);
        assert_eq!(v.len(), 2);
        // PG: 反斜杠不是转义，'a\' 就结束字符串（\\' 后引号关闭）
        let v2 = split_all("INSERT INTO a VALUES ('a\\');\nSELECT 1;", false);
        assert_eq!(v2.len(), 2);
    }

    #[test]
    fn comments_and_versioned() {
        let v = split_all(
            "-- comment;\n/*!40101 SET NAMES utf8mb4 */;\n# sharp;\nSELECT 1; -- trailing\n",
            true,
        );
        // 语句1 = 行注释+版本化注释；语句2 = # 注释+SELECT 1；语句3 = 末尾残留注释
        assert_eq!(v.len(), 3);
        assert!(v[0].contains("SET NAMES"));
        assert!(v[1].contains("SELECT 1"));
    }

    #[test]
    fn delimiter_directive() {
        let sql = "DELIMITER ;;\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END;;\nDELIMITER ;\nSELECT 3;";
        let v = split_all(sql, true);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("CREATE PROCEDURE"));
        assert!(v[1].contains("SELECT 3"));
    }

    #[test]
    fn multiple_statements_one_line() {
        let v = split_all("SELECT 1; SELECT 2;  ; SELECT 3;", true);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn unicode_content() {
        let v = split_all("INSERT INTO t VALUES ('中文;内容');\nSELECT '日本語';", true);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("中文;内容"));
        assert!(v[1].contains("日本語"));
    }

    #[test]
    fn parse_create_table_variants() {
        let (name, _, ine) = parse_create_table("CREATE TABLE `my_db`.`users` (id INT);").unwrap();
        assert_eq!(name, "`my_db`.`users`");
        assert!(!ine);

        let (name, _, ine) =
            parse_create_table("-- c\nCREATE TABLE IF NOT EXISTS \"public\".\"t\" (id int);").unwrap();
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

