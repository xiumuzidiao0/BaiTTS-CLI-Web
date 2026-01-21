use anyhow::{Context, Result, anyhow};
use epub::doc::EpubDoc;
use regex::Regex;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Chapter {
    pub title: String,
    pub content: String,
}

#[derive(Debug)]
pub struct Book {
    pub title: String,
    pub chapters: Vec<Chapter>,
    pub cover: Option<(Vec<u8>, String)>, // (data, mime_type)
}

pub fn extract_text(path: &Path) -> Result<Book> {
    let extension = path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "txt" => extract_txt(path),
        "epub" => extract_epub(path),
        _ => Err(anyhow!("不支持的文件格式: {}", extension)),
    }
}

fn extract_txt(path: &Path) -> Result<Book> {
    let content = fs::read_to_string(path).context(format!("无法读取文件内容: {:?}", path))?;
    let book_title = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    
    let mut chapters = Vec::new();
    let chapter_regex = Regex::new(r"(?m)^\s*第[0-9零一二三四五六七八九十百千万]+[章回卷节].*").unwrap();

    let mut current_title = "序章/开始".to_string();
    let mut current_content = String::new();
    let mut found_first_chapter = false;

    for line in content.lines() {
        let trim_line = line.trim();
        if trim_line.is_empty() {
            continue;
        }

        if chapter_regex.is_match(line) {
            if !current_content.is_empty() {
                chapters.push(Chapter {
                    title: current_title.clone(),
                    content: current_content.clone(),
                });
                current_content.clear();
            }
            current_title = trim_line.to_string();
            found_first_chapter = true;
        } else {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    if !current_content.is_empty() {
        chapters.push(Chapter {
            title: current_title,
            content: current_content,
        });
    }

    if chapters.is_empty() && !content.trim().is_empty() {
        chapters.push(Chapter {
            title: book_title.clone(),
            content,
        });
    } else if chapters.len() == 1 && !found_first_chapter {
        chapters[0].title = book_title.clone();
    }

    Ok(Book {
        title: book_title,
        chapters,
        cover: None,
    })
}

fn extract_epub(path: &Path) -> Result<Book> {
    let mut doc = EpubDoc::new(path).map_err(|e| anyhow!("打开 EPUB 文件失败: {}", e))?;
    
    let title = doc.metadata.iter()
        .find(|item| item.property == "title")
        .map(|item| item.value.clone())
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().to_string());

    let cover = doc.get_cover();
    
    let mut chapters = Vec::new();
    let num_chapters = doc.get_num_chapters();

    for i in 0..num_chapters {
        if doc.set_current_chapter(i) {
            if let Some((content, _)) = doc.get_current_str() {
                let text = html2text::from_read(content.as_bytes(), 9999);
                if text.trim().is_empty() { continue; }

                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                let final_title = if let Some(first_line) = lines.first() {
                    if first_line.len() < 50 { // 标题长度限制放宽一些
                        first_line.trim().to_string()
                    } else {
                        format!("第 {} 节", i + 1)
                    }
                } else {
                    format!("第 {} 节", i + 1)
                };

                chapters.push(Chapter {
                    title: final_title,
                    content: text,
                });
            }
        }
    }
    
    if chapters.is_empty() {
        return Err(anyhow!("未从 EPUB 中提取到任何文本"));
    }

    Ok(Book {
        title,
        chapters,
        cover,
    })
}
