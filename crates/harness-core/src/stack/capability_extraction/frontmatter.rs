pub(super) fn yaml_front_matter(text: &str) -> Result<Option<&str>, ()> {
    let first_end = text.find('\n').map_or(text.len(), |index| index + 1);
    let opening = text[..first_end].trim();
    if opening != "---" {
        return Ok(None);
    }
    let rest = &text[first_end..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim() == "---" {
            return Ok(Some(&rest[..offset]));
        }
        offset += line.len();
    }
    Ok(Some(rest))
}

#[cfg(test)]
mod tests {
    use super::yaml_front_matter;

    #[test]
    fn matches_rule_engine_delimiter_semantics() {
        assert_eq!(
            yaml_front_matter(" ---  \ncapabilities: [network]\n\t--- \nbody"),
            Ok(Some("capabilities: [network]\n"))
        );
        assert_eq!(
            yaml_front_matter("---oops\ncapabilities: [network]\n---\n"),
            Ok(None)
        );
        assert_eq!(
            yaml_front_matter("---\ncapabilities: [network]\n...\nbody"),
            Ok(Some("capabilities: [network]\n...\nbody"))
        );
    }
}
