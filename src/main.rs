// src/main.rs

mod api;
mod args;
mod extractor;
mod lrc;
mod process;
mod utils;
mod server;

use anyhow::Result;
use args::Cli;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    // WebUI 模式
    if args.web {
        let api_url = std::env::var("API_URL").ok();
        return server::start_server(5688, api_url).await;
    }

    // 检查 API 参数 (CLI 模式下必须)
    if args.api.is_none() {
        eprintln!("错误: 必须提供 --api 参数 (例如 --api http://127.0.0.1:8774)");
        eprintln!("或者使用 --web 启动 Web 界面");
        std::process::exit(1);
    }
    let api_url = args.api.as_ref().unwrap();

    let client = api::ApiClient::new(api_url.clone())?;
    let blacklist_regex = if let Some(source) = &args.blacklist {
        Some(utils::load_blacklist(source)?)
    } else {
        None
    };

    if args.list {
        let voices = client.fetch_voices().await?;
        println!("可用的声音列表：");
        for voice in voices {
            println!("==================================================");
            println!("ID: {},", voice.id);
            println!("名称: {},", voice.name);
            println!("性别: {},", voice.gender);
            println!("语言: {},", voice.locale);
            println!("类型: {}", voice.voice_type);
        }
        println!("==================================================");
        return Ok(());
    }

    if let Some(file_path) = &args.file {
        process::process_file(file_path, &args, &client, &blacklist_regex, |event| {
            if let process::ProcessEvent::Log(msg) = event { println!("{}", msg); }
        }, || async { false }).await?;
    } else if let Some(dir_path) = &args.dir {
        process::process_directory(dir_path, &args, &client, &blacklist_regex, |event| {
            if let process::ProcessEvent::Log(msg) = event { println!("{}", msg); }
        }).await?;
    } else {
        // 如果没有匹配到任何操作模式 (但这实际上会被 clap 拦截，或者上面检查过了)
        println!("请使用 --help 查看使用说明");
    }

    Ok(())
}
