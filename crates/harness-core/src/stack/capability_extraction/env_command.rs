use super::static_patterns::is_assignment;

const MAX_SPLIT_EXPANSIONS: usize = 16;

pub(super) enum Resolution {
    Command(Vec<String>),
    None,
    Ambiguous,
}

enum DecodeError {
    Invalid,
    Ambiguous,
}

enum Expansion {
    Changed,
    Unchanged,
    Invalid,
    Ambiguous,
}

pub(super) fn resolve(tokens: &[String]) -> Resolution {
    let mut rebuilt = tokens.to_vec();
    for expansion in 0..=MAX_SPLIT_EXPANSIONS {
        match expand_split_option(&mut rebuilt) {
            Expansion::Changed if expansion < MAX_SPLIT_EXPANSIONS => continue,
            Expansion::Changed => return Resolution::Ambiguous,
            Expansion::Invalid => return Resolution::None,
            Expansion::Ambiguous => return Resolution::Ambiguous,
            Expansion::Unchanged => {
                return command_start(&rebuilt)
                    .map(|start| Resolution::Command(rebuilt[start..].to_vec()))
                    .unwrap_or(Resolution::None)
            }
        }
    }
    Resolution::Ambiguous
}

fn expand_split_option(tokens: &mut Vec<String>) -> Expansion {
    let mut index = 1;
    while let Some(token) = tokens.get(index).cloned() {
        if token == "--" || !token.starts_with('-') || is_assignment(&token) {
            return Expansion::Unchanged;
        }
        if token == "--split-string" {
            let Some(payload) = tokens.get(index + 1).cloned() else {
                return Expansion::Invalid;
            };
            return replace_split(tokens, index, index + 2, String::new(), &payload);
        }
        if let Some(payload) = token.strip_prefix("--split-string=") {
            return replace_split(tokens, index, index + 1, String::new(), payload);
        }
        if token.starts_with("--") {
            match long_option_advance(&token) {
                Some(advance) if index + advance <= tokens.len() => {
                    index += advance;
                    continue;
                }
                _ => return Expansion::Invalid,
            }
        }
        let cluster = &token[1..];
        let mut value_option = None;
        for (offset, option) in cluster.char_indices() {
            if matches!(option, 'u' | 'C' | 'P' | 'S') {
                value_option = Some((offset, option));
                break;
            }
            if !matches!(option, '0' | 'i' | 'v') {
                return Expansion::Invalid;
            }
        }
        let Some((offset, option)) = value_option else {
            index += 1;
            continue;
        };
        let value_start = 1 + offset + option.len_utf8();
        let attached = &token[value_start..];
        if option == 'S' {
            let (payload, suffix) = if attached.is_empty() {
                let Some(payload) = tokens.get(index + 1).cloned() else {
                    return Expansion::Invalid;
                };
                (payload, index + 2)
            } else {
                (attached.to_owned(), index + 1)
            };
            return replace_split(
                tokens,
                index,
                suffix,
                token[..value_start - 1].to_owned(),
                &payload,
            );
        }
        index += if attached.is_empty() { 2 } else { 1 };
        if index > tokens.len() {
            return Expansion::Invalid;
        }
    }
    Expansion::Unchanged
}

fn replace_split(
    tokens: &mut Vec<String>,
    index: usize,
    suffix: usize,
    prefix: String,
    payload: &str,
) -> Expansion {
    let decoded = match decode(payload) {
        Ok(tokens) => tokens,
        Err(DecodeError::Invalid) => return Expansion::Invalid,
        Err(DecodeError::Ambiguous) => return Expansion::Ambiguous,
    };
    let mut replacement = Vec::new();
    if prefix != "-" && !prefix.is_empty() {
        replacement.push(prefix);
    }
    replacement.extend(decoded);
    tokens.splice(index..suffix, replacement);
    Expansion::Changed
}

fn long_option_advance(token: &str) -> Option<usize> {
    match token {
        "--ignore-environment" | "--null" | "--debug" => Some(1),
        "--unset" | "--chdir" => Some(2),
        _ if token.starts_with("--unset=") || token.starts_with("--chdir=") => Some(1),
        _ => None,
    }
}

fn command_start(tokens: &[String]) -> Option<usize> {
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            index += 1;
            break;
        }
        if !token.starts_with('-') || is_assignment(token) {
            break;
        }
        if token.starts_with("--") {
            index += long_option_advance(token)?;
            if index > tokens.len() {
                return None;
            }
            continue;
        }
        let cluster = &token[1..];
        let mut advance = 1;
        for (offset, option) in cluster.char_indices() {
            if matches!(option, 'u' | 'C' | 'P' | 'S') {
                let attached = offset + option.len_utf8() < cluster.len();
                advance += usize::from(!attached);
                break;
            }
            if !matches!(option, '0' | 'i' | 'v') {
                return None;
            }
        }
        index += advance;
        if index > tokens.len() {
            return None;
        }
    }
    while tokens.get(index).is_some_and(|token| is_assignment(token)) {
        index += 1;
    }
    (index < tokens.len()).then_some(index)
}

fn decode(payload: &str) -> Result<Vec<String>, DecodeError> {
    let chars = payload.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut word_started = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' {
            let next = *chars.get(index + 1).ok_or(DecodeError::Invalid)?;
            if quote == Some('\'') && !matches!(next, '\\' | '\'') {
                current.push('\\');
                index += 1;
                word_started = true;
                continue;
            }
            match next {
                'c' if quote == Some('"') => return Err(DecodeError::Invalid),
                'c' => break,
                '_' if quote.is_none() => {
                    if word_started {
                        tokens.push(std::mem::take(&mut current));
                        word_started = false;
                    }
                }
                '_' => {
                    current.push(' ');
                    word_started = true;
                }
                'f' => {
                    current.push('\u{000c}');
                    word_started = true;
                }
                'n' => {
                    current.push('\n');
                    word_started = true;
                }
                'r' => {
                    current.push('\r');
                    word_started = true;
                }
                't' => {
                    current.push('\t');
                    word_started = true;
                }
                'v' => {
                    current.push('\u{000b}');
                    word_started = true;
                }
                '#' | '$' | '"' | '\'' | '\\' => {
                    current.push(next);
                    word_started = true;
                }
                ' ' | '\t' => {
                    current.push(next);
                    word_started = true;
                }
                _ => return Err(DecodeError::Invalid),
            }
            index += 2;
            continue;
        }
        if ch == '$' && quote != Some('\'') {
            if chars.get(index + 1) != Some(&'{') {
                return Err(DecodeError::Invalid);
            }
            let Some(close) = chars[index + 2..]
                .iter()
                .position(|ch| *ch == '}')
                .map(|offset| index + 2 + offset)
            else {
                return Err(DecodeError::Invalid);
            };
            let name = chars[index + 2..close].iter().collect::<String>();
            let mut name_chars = name.chars();
            if !name_chars
                .next()
                .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
                || !name_chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            {
                return Err(DecodeError::Invalid);
            }
            return Err(DecodeError::Ambiguous);
        }
        if ch == '#' && quote.is_none() && !word_started {
            break;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                current.push(ch);
            }
            word_started = true;
        } else if matches!(ch, ' ' | '\t') && quote.is_none() {
            if word_started {
                tokens.push(std::mem::take(&mut current));
                word_started = false;
            }
        } else {
            current.push(ch);
            word_started = true;
        }
        index += 1;
    }
    if quote.is_some() {
        return Err(DecodeError::Invalid);
    }
    if word_started {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::{resolve, Resolution};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn resolves_the_sixteenth_split_before_the_stability_check() {
        let mut tokens = vec!["env".to_owned()];
        tokens.extend(std::iter::repeat_n("-S".to_owned(), 16));
        tokens.push("curl".to_owned());
        assert!(matches!(resolve(&tokens), Resolution::Command(command) if command == ["curl"]));
    }

    #[test]
    fn rejects_invalid_substitutions_and_value_option_clusters() {
        for values in [
            &["env", "-S", "${}", "rm"][..],
            &["env", "-S", "$NAME", "rm"][..],
            &["env", "-S", "c\\url", "rm"][..],
        ] {
            assert!(matches!(resolve(&args(values)), Resolution::None));
        }
        assert!(
            matches!(resolve(&args(&["env", "-uS", "printf"])), Resolution::Command(command) if command == ["printf"])
        );
        assert!(
            matches!(resolve(&args(&["env", "-PS", "printf"])), Resolution::Command(command) if command == ["printf"])
        );
    }
}
