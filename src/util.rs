// Escape text so it can be sent in a discord embed without having any markdown formatting.
// For example, '_' becomes '\_'
pub fn escape_discord_markdown(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' => "\\\\".to_owned(),
            '*' => "\\*".to_owned(),
            '_' => "\\_".to_owned(),
            '~' => "\\~".to_owned(),
            '`' => "\\`".to_owned(),
            '|' => "\\|".to_owned(),
            '>' => "\\>".to_owned(),
            c => c.to_string(),
        })
        .collect()
}
