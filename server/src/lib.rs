use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::thread;

use engine::Engine;
use tauri::Manager as _;

mod chess;
mod common;
mod config;
mod engine;
mod listen;
mod logger;
mod worker;
mod yolo;

// 全局共享状态，用Arc和Mutex包装以实现线程安全共享
struct SharedState {
    config: Arc<RwLock<config::Config>>,
    engine: Arc<Mutex<Engine>>,
    listen_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

static SHARED_STATE: OnceLock<SharedState> = OnceLock::new();

fn get_lib_path<R: tauri::Runtime, M: tauri::Manager<R>>(manager: &M) -> std::path::PathBuf {
    let mut lib_path = manager.path().resolve("../libs/pikafish", tauri::path::BaseDirectory::Resource).unwrap();
    if !lib_path.exists() {
        if let Ok(exe_path) = std::env::current_exe() {
            let mut path = exe_path.clone();
            while path.pop() {
                let candidate = path.join("libs/pikafish");
                if candidate.exists() {
                    lib_path = candidate;
                    break;
                }
                let candidate_parent = path.join("../libs/pikafish");
                if candidate_parent.exists() {
                    lib_path = candidate_parent;
                    break;
                }
            }
        }
    }
    lib_path
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            logger::init_tracer(tracing::Level::DEBUG, &app.path().app_data_dir().unwrap());

            let _ = SHARED_STATE.get_or_init(|| {
                let config = config::Config::load(&app.path().config_dir().unwrap());
                let lib_path = get_lib_path(app);
                let mut engine = engine::Engine::new(&lib_path);
                engine.set_show_wdl(config.engine.show_wdl);
                engine.set_hash(config.engine.hash);
                engine.set_threads(config.engine.threads);

                SharedState {
                    config: Arc::new(RwLock::new(config)),
                    engine: Arc::new(Mutex::new(engine)),
                    listen_thread: Mutex::new(None),
                }
            });

            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            reload_engine,
            listen::list_windows,
            worker::start_listen,
            worker::stop_listen,
            config::get_engine_config,
            config::set_engine_depth,
            config::set_engine_time,
            config::set_engine_threads,
            config::set_engine_hash,
            config::set_chessdb,
            config::get_autoplay,
            config::set_autoplay,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn reload_engine(app: tauri::AppHandle) {
    let lib_path = get_lib_path(&app);
    let state = SHARED_STATE.get().unwrap();
    let engine_config = state.config.read().unwrap().engine;
    state.engine.lock().unwrap().reload(&lib_path, &engine_config);
}
