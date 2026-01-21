

// 查找理想的分割点（各类标点符号或空格）。
fn is_break_character(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '？' | '！' | '；' | '：' | '、' |
        ',' | '.' | '?' | '!' | ';' | ':' |
        '“' | '”' | '‘' | '’' | '【' | '】' | '（' | '）' | '《' | '》' | '『' | '』' |
        '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' |
        ' '
    )
}

// 智能地按字符数分割行，避免在行首出现标点符号。
pub fn split_line_intelligently(line: &str, max_chars: usize) -> Vec<String> {
    if line.chars().count() <= max_chars {
        return vec![line.to_string()];
    }

    let mut chunks = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut current_pos = 0;

    while current_pos < chars.len() && (chars[current_pos].is_whitespace() || is_break_character(chars[current_pos])) {
        current_pos += 1;
    }

    while current_pos < chars.len() {
        if chars.len() - current_pos <= max_chars {
            let chunk = chars[current_pos..].iter().collect::<String>();
            let trimmed_chunk = chunk.trim();
            if !trimmed_chunk.is_empty() {
                chunks.push(trimmed_chunk.to_string());
            }
            break;
        }

        let search_end = current_pos + max_chars;
        let search_range = &chars[current_pos..search_end];

        if let Some(split_pos_relative) = search_range.iter().rposition(|&c| is_break_character(c)) {
            let split_pos_absolute = current_pos + split_pos_relative;
            let chunk = chars[current_pos..=split_pos_absolute].iter().collect::<String>().trim().to_string();
            if !chunk.is_empty() { chunks.push(chunk); }
            
            let mut next_pos = split_pos_absolute + 1;
            while next_pos < chars.len() && (chars[next_pos].is_whitespace() || is_break_character(chars[next_pos])) {
                next_pos += 1;
            }
            current_pos = next_pos;
        } else if let Some(next_break_relative) = chars[search_end..].iter().position(|&c| is_break_character(c)) {
            let split_pos_absolute = search_end + next_break_relative;
            let chunk = chars[current_pos..=split_pos_absolute].iter().collect::<String>().trim().to_string();
            if !chunk.is_empty() { chunks.push(chunk); }
            
            let mut next_pos = split_pos_absolute + 1;
            while next_pos < chars.len() && (chars[next_pos].is_whitespace() || is_break_character(chars[next_pos])) {
                next_pos += 1;
            }
            current_pos = next_pos;
        } else {
            let chunk = chars[current_pos..].iter().collect::<String>().trim().to_string();
            if !chunk.is_empty() { chunks.push(chunk); }
            break;
        }
    }
    chunks
}
