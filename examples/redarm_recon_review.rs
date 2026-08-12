//! Redarm: feed a recon file to Claude (pipe mode) for an independent
//! defensive review — find missed vector combinations, bugs, vulns.
//!
//! Usage:
//!   cargo run --example redarm_recon_review -- <recon_file> [out_file]

use gate4agent::pipe::{create_ndjson_parser, CliEvent, PipeProcess, PipeProcessOptions};
use gate4agent::CliTool;
use std::time::{Duration, Instant};

const PROMPT_TEMPLATE: &str = r#"Ты — независимый defensive-ревьюер результатов редтима. Ниже — полный recon-файл авторизованного тестирования (заказчик = владелец периметра).

Твоя задача — НЕ пересказывать документ, а найти то, что команда могла УПУСТИТЬ:

1. Наложения векторов: комбинации двух+ находок, каждая из которых по отдельности тупиковая, но вместе дают новый путь (примеры паттернов: чтение + запись, слепая примитив + оракул через ошибки/тайминги, кросс-контурные переходы, цепочки через доверенных потребителей контента).
2. Противоречия и пробелы в самих выводах: где команда сделала вывод «мёртв/закрыто» на слабых основаниях, что стоит перепроверить.
3. Классы багов, которые по стеку/архитектуре должны существовать, но в документе не исследованы вовсе.
4. Что из «мертвого» оживает при смене предположений (другая роль, другой контур, race, state confusion).

Формат заключения (строго):
- Топ гипотез, отсортированные по (вероятность × импакт). Для каждой: суть в 2-3 предложениях, какие факты из recon её подпитывают (со ссылками на секции), какая ОДНА минимальная проверка её подтверждает или убивает, и в какие ROE-раму укладывается (пассив/одиночная проба/нужен ROE-лифт).
- Отдельным блоком: «что перепроверить из закрытого» — список с причиной сомнения.
- Без воды, без общих советов уровня «используйте CSP». Только то, что следует из ЭТОГО документа.

Документ ниже между маркерами ===BEGIN RECON=== и ===END RECON===.

"#;

fn main() {
    let mut args = std::env::args().skip(1);
    let recon_path = args.next().unwrap_or_else(|| {
        eprintln!("usage: redarm_recon_review <recon_file> [out_file]");
        std::process::exit(2);
    });
    let out_path = args
        .next()
        .unwrap_or_else(|| format!("{}.claude-review.md", recon_path));

    let recon = std::fs::read_to_string(&recon_path).expect("failed to read recon file");
    let prompt = format!(
        "{}===BEGIN RECON===\n{}\n===END RECON===\n",
        PROMPT_TEMPLATE, recon
    );
    println!(
        "recon: {} ({} bytes), out: {}, prompt: {} bytes",
        recon_path,
        recon.len(),
        out_path,
        prompt.len()
    );

    let cwd = std::env::current_dir().unwrap();
    let mut options = PipeProcessOptions::default();
    // Model override: G4A_CLAUDE_MODEL=sonnet (opus may safeguard-flag
    // even sanitized recon docs; default keeps historical behaviour).
    options.claude.model = Some(
        std::env::var("G4A_CLAUDE_MODEL").unwrap_or_else(|_| "opus".to_string()),
    );

    let mut pipe = PipeProcess::new_with_options(CliTool::ClaudeCode, &cwd, &prompt, options)
        .expect("Failed to spawn Claude pipe process");

    println!("Claude spawned (opus), streaming...\n");

    let mut parser = create_ndjson_parser(CliTool::ClaudeCode);
    let start = Instant::now();
    let timeout = Duration::from_secs(1800); // 30 min
    let mut full_text = String::new();

    loop {
        if start.elapsed() > timeout {
            eprintln!("\n[TIMEOUT after 30min]");
            break;
        }

        if let Some(line) = pipe.try_recv() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let events = parser.parse_line(trimmed);
            for event in events {
                match &event {
                    CliEvent::AssistantText { text, is_delta } => {
                        if *is_delta {
                            print!("{}", text);
                        } else if full_text.len() < text.len() {
                            full_text = text.clone();
                        }
                    }
                    CliEvent::SessionStart { session_id, model, .. } => {
                        println!("[SESSION] id={} model={}", session_id, model);
                    }
                    CliEvent::TurnComplete { input_tokens, output_tokens, .. } => {
                        println!("\n[TOKENS] in={} out={}", input_tokens, output_tokens);
                    }
                    CliEvent::SessionEnd { result, cost_usd, is_error } => {
                        println!("\n[END] error={} cost={:?}", is_error, cost_usd);
                        if result.len() > full_text.len() {
                            full_text = result.clone();
                        }
                        if !full_text.is_empty() {
                            std::fs::write(&out_path, &full_text).expect("failed to write out file");
                            println!("[WROTE] {} ({} bytes)", out_path, full_text.len());
                        }
                        return;
                    }
                    CliEvent::Error { message } => {
                        eprintln!("[ERROR] {}", message);
                    }
                    _ => {}
                }
            }
        }

        if !pipe.is_running() {
            println!("\n[process exited]");
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if !full_text.is_empty() {
        std::fs::write(&out_path, &full_text).expect("failed to write out file");
        println!("[WROTE] {} ({} bytes)", out_path, full_text.len());
    }
}
