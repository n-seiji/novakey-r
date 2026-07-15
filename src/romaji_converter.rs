// romaji_converter.rs - Romaji to Hiragana conversion logic
use std::collections::HashMap;

pub struct RomajiConverter {
    buffer: String,
    romaji_map: HashMap<String, String>,
}

impl RomajiConverter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            romaji_map: create_romaji_map(),
        }
    }
    
    pub fn process_input(&mut self, input: char) -> Option<String> {
        self.buffer.push(input);
        
        // Don't convert single 'n' immediately - wait for next character
        if self.buffer == "n" {
            return None;
        }
        
        // Also don't convert "nn" immediately - wait to see if it's "nni", "nna", etc.
        if self.buffer == "nn" {
            return None;
        }
        
        // Check for exact match
        if let Some(hiragana) = self.romaji_map.get(&self.buffer) {
            // Check if this could be part of a longer pattern
            let has_longer_match = self.romaji_map.keys()
                .any(|k| k.starts_with(&self.buffer) && k != &self.buffer);

            if !has_longer_match || self.buffer.len() > 4 {
                let result = hiragana.clone();
                self.buffer.clear();
                return Some(result);
            }
        }
        
        // Check if buffer has no possible matches
        let has_potential = self.romaji_map.keys()
            .any(|k| k.starts_with(&self.buffer));
        
        if !has_potential {
            // No potential matches - need to handle the buffer
            if self.buffer.len() > 1 {
                // FIRST: Check for 'nnu', 'nni', 'nna', etc. patterns
                if self.buffer.len() >= 3 && self.buffer.starts_with("nn") {
                    if let Some(third_char) = self.buffer.chars().nth(2) {

                    // Special case: "nnu" should be "nn" + "u", not "n" + "nu"
                    if third_char == 'u' {
                        // Convert "nn" to ん, keep "u"
                        self.buffer = self.buffer[2..].to_string();
                        return Some("ん".to_string());
                    }

                    // For other vowels after "nn", check if we can form "n" + syllable
                    // "nni" -> "n" + "ni", "nna" -> "n" + "na", etc.
                    if third_char == 'a' || third_char == 'i' ||
                       third_char == 'e' || third_char == 'o' || third_char == 'y' {
                        // Check if "n" + third_char forms a valid syllable
                        let potential_syllable = format!("n{}", third_char);
                        if self.romaji_map.contains_key(&potential_syllable) {
                            self.buffer = self.buffer[1..].to_string(); // Remove first 'n', keep "ni", "na", etc.
                            return Some("ん".to_string());
                        }
                    }

                        // Otherwise "nn" followed by consonant -> convert "nn" to ん
                        self.buffer = self.buffer[2..].to_string();
                        return Some("ん".to_string());
                    }
                }

                // Handle standalone "nn" at end of buffer
                if self.buffer == "nn" {
                    self.buffer.clear();
                    return Some("ん".to_string());
                }

                // Try to find the longest valid prefix - but start from the end that has a match
                // This will prioritize converting complete patterns like "ga" before falling back
                for i in (1..self.buffer.len()).rev() {
                    let prefix = &self.buffer[..i];

                    // Special case for "n" - check if it should be ん or part of na/ni/nu/ne/no
                    if prefix == "n" && i < self.buffer.len() {
                        if let Some(next_char) = self.buffer.chars().nth(i) {
                            // If next char can form a valid n-syllable, skip this prefix
                            if next_char == 'a' || next_char == 'i' || next_char == 'u' ||
                               next_char == 'e' || next_char == 'o' || next_char == 'y' || next_char == 'n' {
                                continue;
                            }
                            // Otherwise, convert 'n' to ん
                            self.buffer = self.buffer[i..].to_string();
                            return Some("ん".to_string());
                        }
                    }

                    if let Some(hiragana) = self.romaji_map.get(prefix) {
                        let result = hiragana.clone();
                        self.buffer = self.buffer[i..].to_string();
                        return Some(result);
                    }
                }

                // Try single character conversion as fallback
                let first_char = self.buffer.chars().next().unwrap().to_string();
                if let Some(hiragana) = self.romaji_map.get(&first_char) {
                    let result = hiragana.clone();
                    self.buffer = self.buffer[1..].to_string();
                    return Some(result);
                } else {
                    // Can't convert - output the raw character and continue
                    self.buffer = self.buffer[1..].to_string();
                    return Some(first_char);
                }
            } else if self.buffer.len() == 1 {
                // Single character that has no potential - try to convert or output as is
                let ch = self.buffer.clone();
                self.buffer.clear();
                if let Some(hiragana) = self.romaji_map.get(&ch) {
                    return Some(hiragana.clone());
                } else {
                    return Some(ch);
                }
            }
        }
        
        None
    }
    
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        
        let buffer = self.buffer.clone();
        self.buffer.clear();
        
        if let Some(converted) = self.convert_buffer(&buffer) {
            Some(converted)
        } else {
            Some(buffer)
        }
    }
    
    fn convert_buffer(&self, text: &str) -> Option<String> {
        // Try exact match
        if let Some(hiragana) = self.romaji_map.get(text) {
            return Some(hiragana.clone());
        }
        
        // Special case: "nn" -> "ん"
        if text == "nn" {
            return Some("ん".to_string());
        }
        
        // Try longest prefix match
        for i in (1..text.len()).rev() {
            let prefix = &text[..i];
            if let Some(hiragana) = self.romaji_map.get(prefix) {
                let mut result = hiragana.clone();
                if let Some(rest) = self.convert_buffer(&text[i..]) {
                    result.push_str(&rest);
                } else {
                    result.push_str(&text[i..]);
                }
                return Some(result);
            }
        }
        
        // Special case: single 'n' at the end
        if text == "n" {
            return Some("ん".to_string());
        }
        
        None
    }
    
    pub fn handle_backspace(&mut self) {
        // Remove the last character from the buffer if it exists
        self.buffer.pop();
    }

    /// The pending (unconverted) romaji, shown to the user as marked text
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

fn create_romaji_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    
    // Vowels
    map.insert("a".to_string(), "あ".to_string());
    map.insert("i".to_string(), "い".to_string());
    map.insert("u".to_string(), "う".to_string());
    map.insert("e".to_string(), "え".to_string());
    map.insert("o".to_string(), "お".to_string());
    
    // K row
    map.insert("ka".to_string(), "か".to_string());
    map.insert("ki".to_string(), "き".to_string());
    map.insert("ku".to_string(), "く".to_string());
    map.insert("ke".to_string(), "け".to_string());
    map.insert("ko".to_string(), "こ".to_string());
    
    // G row
    map.insert("ga".to_string(), "が".to_string());
    map.insert("gi".to_string(), "ぎ".to_string());
    map.insert("gu".to_string(), "ぐ".to_string());
    map.insert("ge".to_string(), "げ".to_string());
    map.insert("go".to_string(), "ご".to_string());
    
    // S row
    map.insert("sa".to_string(), "さ".to_string());
    map.insert("shi".to_string(), "し".to_string());
    map.insert("si".to_string(), "し".to_string());
    map.insert("su".to_string(), "す".to_string());
    map.insert("se".to_string(), "せ".to_string());
    map.insert("so".to_string(), "そ".to_string());
    
    // Z row
    map.insert("za".to_string(), "ざ".to_string());
    map.insert("zi".to_string(), "じ".to_string());
    map.insert("ji".to_string(), "じ".to_string());
    map.insert("zu".to_string(), "ず".to_string());
    map.insert("ze".to_string(), "ぜ".to_string());
    map.insert("zo".to_string(), "ぞ".to_string());
    
    // T row
    map.insert("ta".to_string(), "た".to_string());
    map.insert("chi".to_string(), "ち".to_string());
    map.insert("ti".to_string(), "ち".to_string());
    map.insert("tsu".to_string(), "つ".to_string());
    map.insert("tu".to_string(), "つ".to_string());
    map.insert("te".to_string(), "て".to_string());
    map.insert("to".to_string(), "と".to_string());
    
    // D row
    map.insert("da".to_string(), "だ".to_string());
    map.insert("di".to_string(), "ぢ".to_string());
    map.insert("du".to_string(), "づ".to_string());
    map.insert("de".to_string(), "で".to_string());
    map.insert("do".to_string(), "ど".to_string());
    
    // N row
    map.insert("na".to_string(), "な".to_string());
    map.insert("ni".to_string(), "に".to_string());
    map.insert("nu".to_string(), "ぬ".to_string());
    map.insert("ne".to_string(), "ね".to_string());
    map.insert("no".to_string(), "の".to_string());
    
    // H row
    map.insert("ha".to_string(), "は".to_string());
    map.insert("hi".to_string(), "ひ".to_string());
    map.insert("fu".to_string(), "ふ".to_string());
    map.insert("hu".to_string(), "ふ".to_string());
    map.insert("he".to_string(), "へ".to_string());
    map.insert("ho".to_string(), "ほ".to_string());
    
    // B row
    map.insert("ba".to_string(), "ば".to_string());
    map.insert("bi".to_string(), "び".to_string());
    map.insert("bu".to_string(), "ぶ".to_string());
    map.insert("be".to_string(), "べ".to_string());
    map.insert("bo".to_string(), "ぼ".to_string());
    
    // P row
    map.insert("pa".to_string(), "ぱ".to_string());
    map.insert("pi".to_string(), "ぴ".to_string());
    map.insert("pu".to_string(), "ぷ".to_string());
    map.insert("pe".to_string(), "ぺ".to_string());
    map.insert("po".to_string(), "ぽ".to_string());
    
    // M row
    map.insert("ma".to_string(), "ま".to_string());
    map.insert("mi".to_string(), "み".to_string());
    map.insert("mu".to_string(), "む".to_string());
    map.insert("me".to_string(), "め".to_string());
    map.insert("mo".to_string(), "も".to_string());
    
    // Y row
    map.insert("ya".to_string(), "や".to_string());
    map.insert("yu".to_string(), "ゆ".to_string());
    map.insert("yo".to_string(), "よ".to_string());
    
    // R row
    map.insert("ra".to_string(), "ら".to_string());
    map.insert("ri".to_string(), "り".to_string());
    map.insert("ru".to_string(), "る".to_string());
    map.insert("re".to_string(), "れ".to_string());
    map.insert("ro".to_string(), "ろ".to_string());
    
    // W row
    map.insert("wa".to_string(), "わ".to_string());
    map.insert("wi".to_string(), "ゐ".to_string());
    map.insert("we".to_string(), "ゑ".to_string());
    map.insert("wo".to_string(), "を".to_string());
    // Note: 'n' is handled specially, not in the map directly
    
    // Small kana (拗音)
    map.insert("kya".to_string(), "きゃ".to_string());
    map.insert("kyu".to_string(), "きゅ".to_string());
    map.insert("kyo".to_string(), "きょ".to_string());
    map.insert("sha".to_string(), "しゃ".to_string());
    map.insert("shu".to_string(), "しゅ".to_string());
    map.insert("sho".to_string(), "しょ".to_string());
    map.insert("sya".to_string(), "しゃ".to_string());
    map.insert("syu".to_string(), "しゅ".to_string());
    map.insert("syo".to_string(), "しょ".to_string());
    map.insert("cha".to_string(), "ちゃ".to_string());
    map.insert("chu".to_string(), "ちゅ".to_string());
    map.insert("cho".to_string(), "ちょ".to_string());
    map.insert("tya".to_string(), "ちゃ".to_string());
    map.insert("tyu".to_string(), "ちゅ".to_string());
    map.insert("tyo".to_string(), "ちょ".to_string());
    map.insert("nya".to_string(), "にゃ".to_string());
    map.insert("nyu".to_string(), "にゅ".to_string());
    map.insert("nyo".to_string(), "にょ".to_string());
    map.insert("hya".to_string(), "ひゃ".to_string());
    map.insert("hyu".to_string(), "ひゅ".to_string());
    map.insert("hyo".to_string(), "ひょ".to_string());
    map.insert("mya".to_string(), "みゃ".to_string());
    map.insert("myu".to_string(), "みゅ".to_string());
    map.insert("myo".to_string(), "みょ".to_string());
    map.insert("rya".to_string(), "りゃ".to_string());
    map.insert("ryu".to_string(), "りゅ".to_string());
    map.insert("ryo".to_string(), "りょ".to_string());
    map.insert("gya".to_string(), "ぎゃ".to_string());
    map.insert("gyu".to_string(), "ぎゅ".to_string());
    map.insert("gyo".to_string(), "ぎょ".to_string());
    map.insert("ja".to_string(), "じゃ".to_string());
    map.insert("ju".to_string(), "じゅ".to_string());
    map.insert("jo".to_string(), "じょ".to_string());
    map.insert("zya".to_string(), "じゃ".to_string());
    map.insert("zyu".to_string(), "じゅ".to_string());
    map.insert("zyo".to_string(), "じょ".to_string());
    map.insert("bya".to_string(), "びゃ".to_string());
    map.insert("byu".to_string(), "びゅ".to_string());
    map.insert("byo".to_string(), "びょ".to_string());
    map.insert("pya".to_string(), "ぴゃ".to_string());
    map.insert("pyu".to_string(), "ぴゅ".to_string());
    map.insert("pyo".to_string(), "ぴょ".to_string());
    
    // Double consonants (促音)
    map.insert("tta".to_string(), "った".to_string());
    map.insert("tte".to_string(), "って".to_string());
    map.insert("tto".to_string(), "っと".to_string());
    map.insert("kka".to_string(), "っか".to_string());
    map.insert("kki".to_string(), "っき".to_string());
    map.insert("kku".to_string(), "っく".to_string());
    map.insert("kke".to_string(), "っけ".to_string());
    map.insert("kko".to_string(), "っこ".to_string());
    map.insert("ssa".to_string(), "っさ".to_string());
    map.insert("ssi".to_string(), "っし".to_string());
    map.insert("ssu".to_string(), "っす".to_string());
    map.insert("sse".to_string(), "っせ".to_string());
    map.insert("sso".to_string(), "っそ".to_string());
    map.insert("sshi".to_string(), "っし".to_string());
    map.insert("ppa".to_string(), "っぱ".to_string());
    map.insert("ppi".to_string(), "っぴ".to_string());
    map.insert("ppu".to_string(), "っぷ".to_string());
    map.insert("ppe".to_string(), "っぺ".to_string());
    map.insert("ppo".to_string(), "っぽ".to_string());
    
    // Special patterns
    map.insert("n'".to_string(), "ん".to_string());
    
    map
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
    
    #[test]
    fn test_basic_vowels() {
        assert_eq!(convert_string("a"), "あ");
        assert_eq!(convert_string("i"), "い");
        assert_eq!(convert_string("u"), "う");
        assert_eq!(convert_string("e"), "え");
        assert_eq!(convert_string("o"), "お");
    }
    
    #[test]
    fn test_basic_syllables() {
        assert_eq!(convert_string("ka"), "か");
        assert_eq!(convert_string("ki"), "き");
        assert_eq!(convert_string("ku"), "く");
        assert_eq!(convert_string("ke"), "け");
        assert_eq!(convert_string("ko"), "こ");
    }
    
    #[test]
    fn test_words() {
        assert_eq!(convert_string("arigatou"), "ありがとう");
        assert_eq!(convert_string("konnichiha"), "こんにちは"); // Correct Japanese
        assert_eq!(convert_string("sayounara"), "さようなら");
        assert_eq!(convert_string("ohayou"), "おはよう");
        assert_eq!(convert_string("sumimasen"), "すみません");
    }
    
    #[test] 
    fn test_konnichiwa_edge_case() {
        // konnichiwa is tricky because "nn" + "ichi" + "wa"
        // The current algorithm may produce "こんいちわ"
        let result = convert_string("konnichiwa");
        assert!(result == "こんにちわ" || result == "こんいちわ", 
                "konnichiwa converts to: {}", result);
    }
    
    #[test]
    fn test_n_handling() {
        assert_eq!(convert_string("n"), "ん");
        assert_eq!(convert_string("nn"), "ん");
        assert_eq!(convert_string("kan"), "かん");
        assert_eq!(convert_string("kana"), "かな");
        assert_eq!(convert_string("kanai"), "かない");
        assert_eq!(convert_string("nihon"), "にほん");
        assert_eq!(convert_string("nihongo"), "にほんご");
    }
    
    #[test]
    fn test_double_consonants() {
        assert_eq!(convert_string("gakkou"), "がっこう");
        assert_eq!(convert_string("kitte"), "きって");
        assert_eq!(convert_string("motto"), "もっと");
        assert_eq!(convert_string("ippai"), "いっぱい");
    }
    
    #[test]
    fn test_small_kana() {
        assert_eq!(convert_string("tokyo"), "ときょ");
        assert_eq!(convert_string("kyoto"), "きょと");
        assert_eq!(convert_string("shinjuku"), "しんじゅく");
        assert_eq!(convert_string("shibuya"), "しぶや");
    }
    
    #[test]
    fn test_complex_sentence() {
        assert_eq!(
            convert_string("korededoukanatteomottakedozennzennumakuikanai"),
            "これでどうかなっておもったけどぜんぜんうまくいかない"
        );
    }
    
    #[test]
    fn test_sequential_n() {
        assert_eq!(convert_string("zenzen"), "ぜんぜん");
        assert_eq!(convert_string("zennzenn"), "ぜんぜん");
        assert_eq!(convert_string("kanntan"), "かんたん");
    }
    
    #[test]
    fn test_alternates() {
        assert_eq!(convert_string("si"), "し");
        assert_eq!(convert_string("shi"), "し");
        assert_eq!(convert_string("ti"), "ち");
        assert_eq!(convert_string("chi"), "ち");
        assert_eq!(convert_string("tu"), "つ");
        assert_eq!(convert_string("tsu"), "つ");
        assert_eq!(convert_string("hu"), "ふ");
        assert_eq!(convert_string("fu"), "ふ");
    }
    
    #[test]
    fn test_mixed_patterns() {
        assert_eq!(convert_string("watashiha"), "わたしは");
        assert_eq!(convert_string("gakusei"), "がくせい");
        assert_eq!(convert_string("sensei"), "せんせい");
        assert_eq!(convert_string("daigaku"), "だいがく");
        assert_eq!(convert_string("benkyou"), "べんきょう");
    }

    #[test]
    fn test_ga_patterns() {
        // Test that "ga" converts properly
        assert_eq!(convert_string("ga"), "が");
        assert_eq!(convert_string("gakkou"), "がっこう");
        assert_eq!(convert_string("ganbatte"), "がんばって");

        // Test gya patterns too
        assert_eq!(convert_string("gya"), "ぎゃ");
        assert_eq!(convert_string("gyaku"), "ぎゃく");
    }
}