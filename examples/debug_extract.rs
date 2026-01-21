use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

// 引入项目中的 extractor 模块
// 注意：在 example 中直接引用 main.rs 里的模块比较麻烦，
// 这里我们将 extractor.rs 的逻辑简单复制或作为 lib 引用。
// 为方便测试，这里我们假设已将 lib.rs 拆分，或者简单地再次包含相关逻辑用于演示。
// 但最佳实践是将核心逻辑移至 lib.rs。
// 鉴于目前结构，我们在此通过路径引用临时解决，或者为了演示，重复少量代码。
// *更好的方式* 是稍微重构一下项目结构，把 src/main.rs 里的模块变成库。
// 但为了不大幅改动你的 main.rs 结构，我将在这个脚本中直接使用相同的 crate 依赖。

use epub::doc::EpubDoc;
use std::fs;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 要测试的文件路径 (支持 txt, epub)
    #[arg(short, long)]
    file: PathBuf,
}

#[derive(Debug)]
pub struct Chapter {
    pub title: String,
    pub content: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    println!("正在尝试读取文件: {:?}", args.file);

    let chapters = extract_text(&args.file)?;

    println!("--------------------------------------------------");
    println!("成功提取文本！共 {} 个章节", chapters.len());
    println!("--------------------------------------------------");
    
    // 预览前 3 章
    for (i, chapter) in chapters.iter().take(3).enumerate() {
        println!("章节 {}: {}", i + 1, chapter.title);
        println!("字数: {}", chapter.content.len());
        println!("预览:\n{}", list_first_chars(&chapter.content, 100));
        println!("--------------------------------------------------");
    }

    Ok(())
}

fn list_first_chars(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

fn extract_text(path: &Path) -> Result<Vec<Chapter>> {
    let extension = path.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "txt" => {
             // 简单的 TXT 处理：整个文件作为一个章节
             // 注意：这里没有复制 regex 分章逻辑，只是为了让 example 能跑通
             let content = fs::read_to_string(path).map_err(|e| anyhow::anyhow!("无法读取文件: {}", e))?;
             Ok(vec![Chapter { title: "全文".to_string(), content }])
        },
        "epub" => extract_epub(path),
        _ => Err(anyhow::anyhow!("不支持的文件格式: {}", extension)),
    }
}

fn extract_epub(path: &Path) -> Result<Vec<Chapter>> {
    let mut doc = EpubDoc::new(path).map_err(|e| anyhow::anyhow!("打开 EPUB 文件失败: {}", e))?;
    let mut chapters = Vec::new();

    let num_chapters = doc.get_num_chapters();
    for i in 0..num_chapters {
        if doc.set_current_chapter(i) {
            if let Some((content, _)) = doc.get_current_str() {
                let text = html2text::from_read(content.as_bytes(), 9999);
                if text.trim().is_empty() { continue; }
                
                chapters.push(Chapter {
                    title: format!("Chapter {}", i),
                    content: text,
                });
            }
        }
    }
    
    if chapters.is_empty() {
        return Err(anyhow::anyhow!("未从 EPUB 中提取到任何文本"));
    }

    Ok(chapters)
}
