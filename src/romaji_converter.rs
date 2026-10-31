#![forbid(unsafe_code)]

// romaji_converter.rs - ローマ字→ひらがな変換。表は Google 日本語入力(Mozc)と同じ。
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// 変換表の出力と、出力後にバッファへ持ち越す文字(`tt` → っ + `t`)。
type Rule = (&'static str, &'static str);

struct Table {
    rules: HashMap<&'static str, Rule>,
    /// いずれかのキーの「真の前方部分」になっている文字列(続きを待つ判断に使う)。
    prefixes: HashSet<&'static str>,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| parse_table(include_str!("romaji_table.tsv")));

/// `input<TAB>output[<TAB>pending]` の行を読む。`#` 始まりと空行は読み飛ばす。
fn parse_table(tsv: &'static str) -> Table {
    let rules: HashMap<&'static str, Rule> = tsv
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some((cols.next()?, (cols.next()?, cols.next().unwrap_or(""))))
        })
        .collect();
    let prefixes = rules
        .keys()
        .flat_map(|key| (1..key.len()).map(|n| &key[..n]))
        .collect();
    Table { rules, prefixes }
}

pub struct RomajiConverter {
    buffer: String,
}

impl RomajiConverter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    /// 1 文字を足す。確定したかながあれば返す(続きを待つ間は None)。
    pub fn process_input(&mut self, input: char) -> Option<String> {
        let table = &*TABLE;
        self.buffer.push(input);
        let mut out = String::new();
        while !self.buffer.is_empty() && !table.prefixes.contains(self.buffer.as_str()) {
            if let Some((kana, pending)) = table.rules.get(self.buffer.as_str()) {
                out.push_str(kana);
                self.buffer = pending.to_string();
                continue;
            }
            // buffer は表に続かない。新しい文字を除いた手前側を先に確定して再試行する。
            let split = self.buffer.len() - input.len_utf8();
            let (old, _) = self.buffer.split_at(split);
            let resolved = (1..=old.len())
                .rev()
                .filter(|&n| old.is_char_boundary(n))
                .find_map(|n| table.rules.get(&old[..n]).map(|rule| (n, rule)));
            match resolved {
                Some((n, (kana, pending))) => {
                    out.push_str(kana);
                    self.buffer = format!("{pending}{}", &self.buffer[n..]);
                }
                None => {
                    // 変換できない文字はそのまま出す
                    let first = self.buffer.remove(0);
                    out.push(first);
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// 残りのバッファを確定する。かなと、変換しきれない子音の残り(`enter` の `r` など)をそのまま繋げて返す。何も無ければ None。
    pub fn flush(&mut self) -> Option<String> {
        let table = &*TABLE;
        let mut rest = std::mem::take(&mut self.buffer);
        let mut kana = String::new();
        while let Some((out, pending)) = table.rules.get(rest.as_str()) {
            kana.push_str(out);
            rest = pending.to_string();
        }
        kana.push_str(&rest);
        (!kana.is_empty()).then_some(kana)
    }

    /// 記号 1 文字をローマ字の続きとして受け取れるか (`n'` → ん, `t'i` → てぃ)。
    pub fn accepts(&self, ch: char) -> bool {
        let table = &*TABLE;
        let next = format!("{}{ch}", self.buffer);
        !self.buffer.is_empty()
            && (table.prefixes.contains(next.as_str()) || table.rules.contains_key(next.as_str()))
    }

    pub fn handle_backspace(&mut self) {
        // Remove the last character from the buffer if it exists
        self.buffer.pop();
    }

    /// The pending (unconverted) romaji, shown to the user as marked text
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn set_buffer(&mut self, buffer: &str) {
        self.buffer = buffer.to_string();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_string(input: &str) -> String {
        let mut converter = RomajiConverter::new();
        let mut result = String::new();

        for ch in input.chars() {
            if let Some(output) = converter.process_input(ch) {
                result.push_str(&output);
            }
        }

        if let Some(output) = converter.flush() {
            result.push_str(&output);
        }

        result
    }

    fn pending_after(input: &str) -> String {
        let mut converter = RomajiConverter::new();
        input.chars().for_each(|ch| {
            converter.process_input(ch);
        });
        converter.buffer().to_string()
    }

    /// 入力中に確定したかなと、最後の flush の結果。
    fn run(input: &str) -> (String, Option<String>) {
        let mut converter = RomajiConverter::new();
        let done = input
            .chars()
            .filter_map(|ch| converter.process_input(ch))
            .collect();
        (done, converter.flush())
    }

    fn assert_table(cases: &[(&str, &str)]) {
        for (input, want) in cases {
            assert_eq!(convert_string(input), *want, "input {input:?}");
        }
    }

    #[test]
    fn 表の行数が読み込める() {
        assert!(TABLE.rules.len() >= 300);
    }

    #[test]
    fn 母音と基本の音() {
        assert_table(&[
            ("a", "あ"), ("i", "い"), ("u", "う"), ("e", "え"), ("o", "お"),
            ("ka", "か"), ("ki", "き"), ("ku", "く"), ("ke", "け"), ("ko", "こ"),
            ("si", "し"), ("shi", "し"), ("ti", "ち"), ("chi", "ち"),
            ("tu", "つ"), ("tsu", "つ"), ("hu", "ふ"), ("fu", "ふ"),
            ("ga", "が"), ("gya", "ぎゃ"), ("tokyo", "ときょ"), ("shinjuku", "しんじゅく"),
        ]);
    }

    #[test]
    fn 小書きのかな() {
        assert_table(&[
            ("xa", "ぁ"), ("xi", "ぃ"), ("xu", "ぅ"), ("xe", "ぇ"), ("xo", "ぉ"),
            ("la", "ぁ"), ("li", "ぃ"), ("lu", "ぅ"), ("le", "ぇ"), ("lo", "ぉ"),
            ("xtu", "っ"), ("ltu", "っ"), ("xtsu", "っ"), ("ltsu", "っ"),
            ("xya", "ゃ"), ("xyu", "ゅ"), ("xyo", "ょ"), ("lya", "ゃ"),
            ("xwa", "ゎ"), ("lwa", "ゎ"),
        ]);
    }

    #[test]
    fn 外来語の音() {
        assert_table(&[
            ("fa", "ふぁ"), ("fi", "ふぃ"), ("fe", "ふぇ"), ("fo", "ふぉ"),
            ("thi", "てぃ"), ("dhi", "でぃ"), ("dhu", "でゅ"), ("twu", "とぅ"),
            ("she", "しぇ"), ("je", "じぇ"), ("che", "ちぇ"),
            ("ye", "いぇ"), ("wi", "うぃ"), ("we", "うぇ"), ("wo", "を"),
            ("va", "ゔぁ"), ("vi", "ゔぃ"), ("vu", "ゔ"), ("ve", "ゔぇ"), ("vo", "ゔぉ"),
            ("kwa", "くぁ"), ("tsa", "つぁ"), ("qa", "くぁ"),
        ]);
    }

    #[test]
    fn うぃんどが打てる() {
        assert_eq!(convert_string("uxinndo"), "うぃんど");
        assert_eq!(convert_string("uxindo"), "うぃんど");
    }

    #[test]
    fn 促音は子音を重ねる() {
        assert_table(&[
            ("kka", "っか"), ("cchi", "っち"), ("ssha", "っしゃ"), ("jja", "っじゃ"),
            ("ggo", "っご"), ("dde", "っで"), ("ppo", "っぽ"), ("bba", "っば"),
            ("zzu", "っず"), ("hha", "っは"), ("ffa", "っふぁ"), ("tta", "った"),
            ("rra", "っら"), ("yya", "っや"), ("wwa", "っわ"), ("vvu", "っゔ"),
            ("gakkou", "がっこう"), ("kitte", "きって"), ("motto", "もっと"),
            ("ippai", "いっぱい"), ("matcha", "まっちゃ"),
        ]);
    }

    #[test]
    fn nの扱いはmozcと同じ() {
        assert_table(&[
            ("n", "ん"), ("nn", "ん"), ("n'", "ん"), ("na", "な"), ("nya", "にゃ"),
            ("nni", "んい"), ("nna", "んあ"), ("nnn", "んん"),
            ("kanji", "かんじ"), ("kan", "かん"), ("nihon", "にほん"),
            ("konnichiha", "こんいちは"), ("konnnichiha", "こんにちは"),
            ("kon'nichiha", "こんにちは"), ("ten'in", "てんいん"),
            ("shinbun", "しんぶん"), ("zennzenn", "ぜんぜん"), ("onnna", "おんな"), ("onna", "おんあ"),
            ("sannnin", "さんにん"), ("sannin", "さんいん"), ("hon", "ほん"),
        ]);
    }

    #[test]
    fn nの確定タイミング() {
        assert_eq!(pending_after("n"), "n");
        assert_eq!(pending_after("kon"), "n");
        assert_eq!(pending_after("nn"), "");
        assert_eq!(pending_after("konn"), "");
        assert_eq!(pending_after("ka"), "");
        assert_eq!(pending_after("kk"), "k");
        assert_eq!(run("kan"), ("か".to_string(), Some("ん".to_string())));
    }

    #[test]
    fn 変換できない子音はかなに繋げて返す() {
        assert_eq!(run("enter"), ("えんて".to_string(), Some("r".to_string())));
        assert_eq!(run("sh"), (String::new(), Some("sh".to_string())));
        assert_eq!(run(""), (String::new(), None));
        assert_eq!(run("ka"), ("か".to_string(), None));
    }

    #[test]
    fn 表にない文字はそのまま出る() {
        assert_eq!(convert_string("k1"), "k1");
        assert_eq!(convert_string("kyk"), "kyk");
    }

    #[test]
    fn 単語と文() {
        assert_table(&[
            ("arigatou", "ありがとう"), ("sayounara", "さようなら"), ("sumimasen", "すみません"),
            ("watashiha", "わたしは"), ("ganbatte", "がんばって"), ("benkyou", "べんきょう"),
            ("korededoukanatteomottakedozennzennumakuikanai",
             "これでどうかなっておもったけどぜんぜんうまくいかない"),
        ]);
    }

    #[test]
    fn バックスペースで未確定の末尾が消える() {
        let mut converter = RomajiConverter::new();
        "ky".chars().for_each(|ch| {
            converter.process_input(ch);
        });
        converter.handle_backspace();
        assert_eq!(converter.buffer(), "k");
    }
}
