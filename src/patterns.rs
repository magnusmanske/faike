//! AI vendor, prompt and wikitext signatures.
//!
//! This is the single source of truth for both detection and the generated
//! AbuseFilter rules. Keep the needles conservative: a bare "firefly" or
//! "gemini" is a common word, so free-text matching only uses distinctive
//! phrases. [`match_value`] additionally accepts short exact tool names, for
//! structured fields such as EXIF `Software`.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use std::sync::OnceLock;

/// A vendor, with the literal spellings that identify it in free text.
pub struct Tool {
    pub name: &'static str,
    pub needles: &'static [&'static str],
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "OpenAI DALL·E",
        needles: &[
            "dall-e",
            "dall·e",
            "dall e",
            "dalle",
            "gpt-image",
            "gpt image",
        ],
    },
    Tool {
        name: "OpenAI ChatGPT",
        needles: &["openai", "chatgpt", "gpt-4o", "gpt-5", "openai sora"],
    },
    Tool {
        name: "Midjourney",
        needles: &["midjourney"],
    },
    Tool {
        name: "Stable Diffusion",
        needles: &[
            "stable diffusion",
            "stablediffusion",
            "stable-diffusion",
            "sdxl",
            "sd 1.5",
            "sd1.5",
            "sd 3",
            "stability ai",
            "dreamstudio",
        ],
    },
    Tool {
        name: "ComfyUI",
        needles: &["comfyui", "comfy ui"],
    },
    Tool {
        name: "Automatic1111",
        needles: &[
            "automatic1111",
            "automatic 1111",
            "a1111",
            "stable diffusion webui",
            "stable-diffusion-webui",
            "sd-webui",
            "sd webui",
        ],
    },
    Tool {
        name: "Fooocus",
        needles: &["fooocus"],
    },
    Tool {
        name: "InvokeAI",
        needles: &["invokeai", "invoke ai"],
    },
    Tool {
        name: "NovelAI",
        needles: &["novelai"],
    },
    Tool {
        name: "Leonardo AI",
        needles: &["leonardo.ai", "leonardo ai"],
    },
    Tool {
        name: "Adobe Firefly",
        needles: &[
            "adobe firefly",
            "adobe generative fill",
            "photoshop generative fill",
            "generative fill",
        ],
    },
    Tool {
        name: "Microsoft Designer",
        needles: &[
            "microsoft designer",
            "bing image creator",
            "copilot designer",
            "image creator from microsoft",
        ],
    },
    Tool {
        name: "Google ImageFX",
        needles: &[
            "google imagefx",
            "imagefx",
            "google imagen",
            "imagen 2",
            "imagen 3",
            "imagen 4",
            "google gemini",
            "gemini 2.5",
            "gemini 3",
            "nano banana",
        ],
    },
    Tool {
        name: "Meta AI",
        needles: &["meta ai", "imagine with meta"],
    },
    Tool {
        name: "xAI Grok",
        needles: &["grok imagine", "xai grok", "grok image"],
    },
    Tool {
        name: "Ideogram",
        needles: &["ideogram"],
    },
    Tool {
        name: "FLUX",
        needles: &["flux.1", "flux 1", "black forest labs", "bfl flux"],
    },
    Tool {
        name: "Recraft",
        needles: &["recraft"],
    },
    Tool {
        name: "Krea AI",
        needles: &["krea ai"],
    },
    Tool {
        name: "Playground AI",
        needles: &["playground ai", "playgroundai"],
    },
    Tool {
        name: "Canva AI",
        needles: &["canva ai", "canva magic", "magic media"],
    },
    Tool {
        name: "Craiyon",
        needles: &["craiyon"],
    },
    Tool {
        name: "NightCafe",
        needles: &["nightcafe"],
    },
    Tool {
        name: "DeepAI",
        needles: &["deepai"],
    },
    Tool {
        name: "Dreamina / Jimeng",
        needles: &["dreamina", "jimeng", "即梦"],
    },
    Tool {
        name: "Seedream",
        needles: &["seedream"],
    },
    Tool {
        name: "Kling AI",
        needles: &["kling ai", "klingai", "kling 1", "kling 2", "可灵"],
    },
    Tool {
        name: "Hailuo AI",
        needles: &["hailuo", "海螺"],
    },
    Tool {
        name: "Runway",
        needles: &["runway ml", "runwayml", "runway gen-", "runway gen "],
    },
    Tool {
        name: "Pika",
        needles: &["pika labs", "pika.art", "pika art"],
    },
    Tool {
        name: "Luma AI",
        needles: &["luma ai", "luma dream", "dream machine"],
    },
    Tool {
        name: "PixVerse",
        needles: &["pixverse"],
    },
    Tool {
        name: "Vidu",
        needles: &["vidu ai", "vidu.com"],
    },
    Tool {
        name: "Haiper",
        needles: &["haiper"],
    },
    Tool {
        name: "Genmo",
        needles: &["genmo"],
    },
    Tool {
        name: "Suno",
        needles: &["suno ai", "suno.com"],
    },
    Tool {
        name: "Udio",
        needles: &["udio ai", "udio.com"],
    },
    Tool {
        name: "ElevenLabs",
        needles: &["elevenlabs", "eleven labs"],
    },
    Tool {
        name: "Stable Audio",
        needles: &["stable audio"],
    },
    Tool {
        name: "MusicGen / AudioCraft",
        needles: &["musicgen", "audiocraft"],
    },
    Tool {
        name: "AI upscaler",
        needles: &[
            "topaz gigapixel",
            "gigapixel ai",
            "upscayl",
            "magnific ai",
            "real-esrgan",
            "realesrgan",
            "waifu2x",
            "gfpgan",
            "codeformer",
            "remini",
        ],
    },
    Tool {
        name: "AI editor",
        needles: &["luminar neo", "evoto", "photoroom"],
    },
    Tool {
        name: "Tensor.Art",
        needles: &["tensor.art", "tensorart"],
    },
    Tool {
        name: "Pollinations",
        needles: &["pollinations.ai", "pollinations"],
    },
    Tool {
        name: "Prodia",
        needles: &["prodia"],
    },
    Tool {
        name: "getimg.ai",
        needles: &["getimg.ai"],
    },
    Tool {
        name: "Freepik AI",
        needles: &["freepik ai", "freepik.com/ai"],
    },
    Tool {
        name: "Artbreeder",
        needles: &["artbreeder"],
    },
    Tool {
        name: "This Person Does Not Exist",
        needles: &["this person does not exist", "generated.photos"],
    },
    Tool {
        name: "Stable Assistant",
        needles: &["stable assistant"],
    },
    Tool {
        name: "Chinese AI generators",
        needles: &[
            "tongyi wanxiang",
            "通义万相",
            "hunyuan",
            "混元",
            "doubao",
            "豆包",
        ],
    },
];

/// Short names that are only accepted in structured metadata values (EXIF
/// `Software`, XMP `CreatorTool`, PNG `Software`, ...), never in free text.
pub const SHORT_TOOLS: &[(&str, &str)] = &[
    ("firefly", "Adobe Firefly"),
    ("sora", "OpenAI Sora"),
    ("grok", "xAI Grok"),
    ("gemini", "Google Gemini"),
    ("runway", "Runway"),
    ("pika", "Pika"),
    ("suno", "Suno"),
    ("kling", "Kling AI"),
    ("vidu", "Vidu"),
    ("imagen", "Google Imagen"),
    ("dalle", "OpenAI DALL·E"),
    ("midjourney", "Midjourney"),
    ("comfyui", "ComfyUI"),
    ("invokeai", "InvokeAI"),
    ("novelai", "NovelAI"),
    ("ideogram", "Ideogram"),
    ("recraft", "Recraft"),
    ("krea", "Krea AI"),
];

/// IPTC/C2PA digital source types that indicate AI generation.
///
/// The value is the confidence score of the signal: trained generative media
/// is explicit, while `algorithmicMedia` also covers non-AI algorithms.
pub const AI_SOURCE_TYPES: &[(&str, u8, &str)] = &[
    (
        "trainedAlgorithmicMedia",
        95,
        "trainedAlgorithmicMedia (fully AI-generated)",
    ),
    (
        "compositeWithTrainedAlgorithmicMedia",
        90,
        "compositeWithTrainedAlgorithmicMedia (contains AI-generated parts)",
    ),
    (
        "compositeSynthetic",
        82,
        "compositeSynthetic (contains synthetic parts)",
    ),
    (
        "trainedAlgorithmicData",
        70,
        "trainedAlgorithmicData (AI-generated data)",
    ),
    (
        "algorithmicMedia",
        60,
        "algorithmicMedia (algorithmic, not necessarily AI-trained)",
    ),
    (
        "dataDrivenMedia",
        60,
        "dataDrivenMedia (data-driven generation)",
    ),
];

/// Match an IPTC/C2PA digital source type inside a value (URI or literal).
///
/// The longest matching name wins, so `compositeWithTrainedAlgorithmicMedia`
/// is not reduced to `trainedAlgorithmicMedia`.
pub fn ai_source_type(value: &str) -> Option<(&'static str, u8, &'static str)> {
    let lower = value.to_ascii_lowercase();
    AI_SOURCE_TYPES
        .iter()
        .filter(|(needle, _, _)| lower.contains(&needle.to_ascii_lowercase()))
        .max_by_key(|(needle, _, _)| needle.len())
        .copied()
}

/// Phrases that state AI generation without naming a vendor.
pub const AI_PHRASES: &[&str] = &[
    "ai-generated",
    "ai generated",
    "ai-created",
    "ai created",
    "generated by ai",
    "generated with ai",
    "generated using ai",
    "created by ai",
    "created with ai",
    "created using ai",
    "made with ai",
    "made using ai",
    "with the help of ai",
    "using generative ai",
    "generative ai",
    "genai",
    "text-to-image",
    "text to image",
    "machine-generated",
    "machine generated",
    "synthetic media",
    "synthetic image",
    "ai artwork",
    "ai art",
    "ai-assisted",
    "ai assisted",
];

/// Prompt/parameter markers left by common local generators.
///
/// These are regexes because the values vary. They are also embedded in the
/// generated AbuseFilter rule, so keep them PCRE-compatible.
pub const PROMPT_PATTERNS: &[&str] = &[
    r"\bnegative prompt\s*:",
    r"\bsteps\s*:\s*\d+",
    r"\bsampler\s*:",
    r"\bcfg scale\s*:",
    r"\bseed\s*:\s*\d+",
    r"\bmodel hash\s*:",
    r"\bdenoising strength\s*:",
    r"\bclip skip\s*:",
    r"\bhires upscale\s*:",
    r#""class_type"\s*:"#,
    r#""(ksampler|checkpointloadersimple|cliptextencode|vaedecode|emptylatentimage)"#,
    r#""(positive|negative)_prompt"\s*:"#,
    r#""(ckpt_name|sampler_name|model_name)"\s*:"#,
];

/// Wikitext templates/categories that explicitly label AI content.
pub const WIKITEXT_PATTERNS: &[(&str, &str, u8)] = &[
    (r"\{\{\s*pd[- ]algorithm", "{{PD-algorithm}} template", 95),
    (r"\{\{\s*ai[- ]generated", "AI-generated template", 95),
    (r"\{\{\s*ai[- ]upscaled", "AI-upscaled template", 85),
    (
        r"\{\{\s*created with\s+(midjourney|stable diffusion|dall|adobe firefly|comfyui|flux|ideogram|recraft)",
        "Created with AI tool template",
        85,
    ),
    (
        r"\{\{\s*(generated|created|made)\s+(with|using|by)\s+ai",
        "Explicit AI-generation template",
        85,
    ),
    (
        r"\[\[\s*category\s*:\s*ai[- ]generated",
        "AI-generated category",
        90,
    ),
    (
        r"\[\[\s*category\s*:\s*(images|files)\s+(generated|created)\s+(with|by|using)\s+ai",
        "AI-generated category",
        85,
    ),
    (
        r"\[\[\s*category\s*:\s*(stable diffusion|midjourney|dall-e|comfyui)",
        "AI-tool category",
        85,
    ),
];

struct Patterns {
    tools: AhoCorasick,
    tool_needles: Vec<(&'static str, &'static str)>,
    phrases: AhoCorasick,
    phrase_needles: Vec<&'static str>,
    prompts: RegexSet,
    templates: Vec<(Regex, &'static str, u8)>,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let tool_needles: Vec<(&'static str, &'static str)> = TOOLS
            .iter()
            .flat_map(|tool| tool.needles.iter().map(move |needle| (*needle, tool.name)))
            .collect();
        let tools = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .ascii_case_insensitive(true)
            .build(tool_needles.iter().map(|(needle, _)| *needle))
            .expect("static tool patterns are valid");
        let phrases = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .ascii_case_insensitive(true)
            .build(AI_PHRASES)
            .expect("static AI phrases are valid");
        let prompts = RegexSetBuilder::new(PROMPT_PATTERNS)
            .case_insensitive(true)
            .build()
            .expect("static prompt patterns are valid");
        let templates = WIKITEXT_PATTERNS
            .iter()
            .map(|(pattern, label, weight)| {
                let regex = RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .expect("static wikitext pattern is valid");
                (regex, *label, *weight)
            })
            .collect();
        Patterns {
            tools,
            tool_needles,
            phrases,
            phrase_needles: AI_PHRASES.to_vec(),
            prompts,
            templates,
        }
    })
}

/// Distinctive AI tool mentions in free text.
pub fn match_text(text: &str) -> Vec<(&'static str, &str)> {
    let patterns = patterns();
    let mut out: Vec<(&'static str, &str)> = Vec::new();
    for m in patterns.tools.find_iter(text) {
        let (_, tool) = patterns.tool_needles[m.pattern()];
        if out.iter().any(|(seen, _)| *seen == tool) {
            continue;
        }
        out.push((tool, &text[m.start()..m.end()]));
    }
    out
}

/// Tool mentions in a structured metadata value, allowing short exact names
/// such as `Software: Firefly`.
pub fn match_value(value: &str) -> Vec<(&'static str, &str)> {
    let mut out = match_text(value);
    if !out.is_empty() {
        return out;
    }
    let normalized = value.trim().trim_matches('"').to_lowercase();
    if normalized.is_empty() || normalized.len() > 64 {
        return out;
    }
    for (short, tool) in SHORT_TOOLS {
        if normalized == *short
            || normalized.starts_with(&format!("{short} "))
            || normalized.starts_with(&format!("{short}-"))
        {
            out.push((tool, value));
            break;
        }
    }
    out
}

/// The first explicit AI phrase in `text`, if any.
pub fn ai_phrase(text: &str) -> Option<&'static str> {
    let patterns = patterns();
    patterns
        .phrases
        .find(text)
        .map(|m| patterns.phrase_needles[m.pattern()])
}

/// Prompt markers found in `text`, as distinct regex labels.
pub fn prompt_markers(text: &str) -> Vec<&'static str> {
    patterns()
        .prompts
        .matches(text)
        .iter()
        .map(|index| PROMPT_PATTERNS[index])
        .collect()
}

/// Wikitext templates/categories that mark AI content.
pub fn wikitext_markers(text: &str) -> Vec<(&'static str, u8, String)> {
    let mut out = Vec::new();
    for (regex, label, weight) in &patterns().templates {
        if let Some(m) = regex.find(text) {
            out.push((*label, *weight, truncate(m.as_str(), 120)));
        }
    }
    out
}

/// Regex matching any known AI tool name, for AbuseFilter `irlike`.
pub fn abuse_tool_regex() -> &'static str {
    static REGEX: OnceLock<String> = OnceLock::new();
    REGEX.get_or_init(|| {
        let mut needles: Vec<&str> = TOOLS
            .iter()
            .flat_map(|tool| tool.needles.iter().copied())
            .collect();
        needles.sort_unstable();
        needles.dedup();
        let alternatives: Vec<String> = needles.iter().map(|n| regex::escape(n)).collect();
        format!("(?:{})", alternatives.join("|"))
    })
}

/// Regex matching explicit AI phrases, for AbuseFilter `irlike`.
pub fn abuse_phrase_regex() -> &'static str {
    static REGEX: OnceLock<String> = OnceLock::new();
    REGEX.get_or_init(|| {
        let alternatives: Vec<String> = AI_PHRASES.iter().map(|p| regex::escape(p)).collect();
        format!("(?:{})", alternatives.join("|"))
    })
}

/// Regex matching prompt/parameter markers, for AbuseFilter `irlike`.
pub fn abuse_prompt_regex() -> &'static str {
    static REGEX: OnceLock<String> = OnceLock::new();
    REGEX.get_or_init(|| format!("(?:{})", PROMPT_PATTERNS.join("|")))
}

/// Regex matching AI-labelling templates/categories, for AbuseFilter `irlike`.
pub fn abuse_wikitext_regex() -> &'static str {
    static REGEX: OnceLock<String> = OnceLock::new();
    REGEX.get_or_init(|| {
        let alternatives: Vec<&str> = WIKITEXT_PATTERNS.iter().map(|(p, _, _)| *p).collect();
        format!("(?:{})", alternatives.join("|"))
    })
}

/// Truncate at a character boundary, appending an ellipsis if shortened.
pub fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinctive_free_text_matches() {
        assert_eq!(match_text("made with Midjourney v6")[0].0, "Midjourney");
        assert_eq!(match_text("stable diffusion webui")[0].0, "Automatic1111");
        assert!(
            match_text("ComfyUI workflow")
                .iter()
                .any(|(t, _)| *t == "ComfyUI")
        );
        assert!(
            match_text("Created by Adobe Firefly")
                .iter()
                .any(|(t, _)| *t == "Adobe Firefly")
        );
    }

    #[test]
    fn generic_words_do_not_match_free_text() {
        assert!(match_text("A photo of a firefly at night").is_empty());
        assert!(match_text("Gemini and Sagittarius").is_empty());
        assert!(match_text("Runway at the airport").is_empty());
        assert!(match_text("Canon EOS R5").is_empty());
    }

    #[test]
    fn short_names_match_structured_values() {
        assert_eq!(match_value("Firefly 2.0")[0].0, "Adobe Firefly");
        assert_eq!(match_value("Sora")[0].0, "OpenAI Sora");
        assert_eq!(match_value("Adobe Photoshop").len(), 0);
        assert_eq!(match_value("GIMP").len(), 0);
    }

    #[test]
    fn prompt_markers_are_counted_once_per_kind() {
        let text = "masterpiece, 1girl\nNegative prompt: blurry\nSteps: 30, Sampler: Euler a, CFG scale: 7, Seed: 42";
        assert_eq!(prompt_markers(text).len(), 5);
    }

    #[test]
    fn wikitext_markers_are_detected() {
        let text = "{{Information}}\n{{PD-algorithm}}\n[[Category:AI-generated images]]";
        let markers = wikitext_markers(text);
        assert!(
            markers
                .iter()
                .any(|(label, weight, _)| *label == "{{PD-algorithm}} template" && *weight == 95)
        );
        assert!(
            markers
                .iter()
                .any(|(label, _, _)| *label == "AI-generated category")
        );
    }

    #[test]
    fn abuse_regexes_are_non_empty_and_compile() {
        regex::Regex::new(abuse_tool_regex()).expect("tool regex");
        regex::Regex::new(abuse_phrase_regex()).expect("phrase regex");
        regex::Regex::new(abuse_prompt_regex()).expect("prompt regex");
        regex::Regex::new(abuse_wikitext_regex()).expect("wikitext regex");
    }

    #[test]
    fn truncate_is_unicode_safe() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("日本語", 2), "日本…");
    }
}
