use crate::{platform::CommandPathCache, specs::SpecRegistry};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NaturalLanguageScore {
    pub score: i16,
    pub forced: bool,
    pub reasons: Vec<&'static str>,
}

impl NaturalLanguageScore {
    #[must_use]
    pub const fn should_offer(&self) -> bool {
        self.forced || self.score >= 55
    }
}

#[must_use]
pub fn detect_natural_language(
    text: &str,
    trigger_prefix: &str,
    commands: &CommandPathCache,
    specs: &SpecRegistry,
) -> NaturalLanguageScore {
    let trimmed = text.trim();
    let forced = !trigger_prefix.is_empty() && trimmed.starts_with(trigger_prefix);
    if forced {
        return NaturalLanguageScore {
            score: 100,
            forced: true,
            reasons: vec!["explicit_prefix"],
        };
    }
    let mut score = 0_i16;
    let mut reasons = Vec::new();
    let first = trimmed.split_whitespace().next().unwrap_or_default();
    if commands.contains(first) || specs.get(first).is_some() || first.contains('/') {
        score -= 70;
        reasons.push("known_command");
    }
    if trimmed.contains(['|', '>', '<', ';']) || trimmed.contains("&&") || trimmed.contains("||") {
        score -= 35;
        reasons.push("shell_operator");
    }
    if trimmed.split_whitespace().any(|word| word.starts_with('-')) {
        score -= 25;
        reasons.push("flag_syntax");
    }
    let words = trimmed.split_whitespace().count();
    if words >= 4 {
        score += 30;
        reasons.push("multiple_words");
    } else if words <= 1 {
        score -= 25;
    }
    let chars = trimmed.chars().count().max(1);
    let cjk = trimmed
        .chars()
        .filter(|character| is_cjk(*character))
        .count();
    if cjk * 4 >= chars {
        score += 45;
        reasons.push("cjk_natural_language");
    }
    let lower = trimmed.to_lowercase();
    if [
        "查找",
        "显示",
        "删除",
        "如何",
        "文件",
        "目录",
        "进程",
        "端口",
        "find ",
        "show ",
        "list ",
        "how ",
        "files",
        "directory",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        score += 35;
        reasons.push("intent_words");
    }
    NaturalLanguageScore {
        score: score.clamp(0, 100),
        forced,
        reasons,
    }
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_language_never_needs_network_to_classify() {
        let commands = CommandPathCache::default();
        let specs = SpecRegistry::load(None);
        assert!(
            detect_natural_language("查找当前目录最近修改的文件", "??", &commands, &specs)
                .should_offer()
        );
        assert!(!detect_natural_language("ls -la", "??", &commands, &specs).should_offer());
        assert!(detect_natural_language("?? list files", "??", &commands, &specs).forced);
    }
}
