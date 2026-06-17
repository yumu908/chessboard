use std::thread;
use std::time::Duration;

use tauri::async_runtime::block_on;
use tauri::AppHandle;
use tauri::Emitter as _;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use xcap::image::ImageBuffer;
use xcap::image::Rgba;

use crate::chess;
use crate::common;
use crate::engine::QueryResult;
use crate::listen::ListenWindow;
use crate::listen::Window;
use crate::yolo::predict;
use crate::yolo::IMAGE_HEIGHT;
use crate::yolo::IMAGE_WIDTH;
use crate::SHARED_STATE;

#[cfg(target_os = "windows")]
mod win32 {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct RECT {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    pub struct HWND__(c_void);
    pub type HWND = *mut HWND__;

    unsafe extern "system" {
        pub fn GetWindowRect(hwnd: HWND, lpRect: *mut RECT) -> i32;
        pub fn SetForegroundWindow(hwnd: HWND) -> i32;
        pub fn SetCursorPos(x: i32, y: i32) -> i32;
        pub fn mouse_event(dw_flags: u32, dx: u32, dy: u32, dw_data: u32, dw_extra_info: usize);
        pub fn IsWindow(hwnd: HWND) -> i32;
    }

    pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
}

// 棋盘分析结果
struct BoardAnalysisResult {
    expect_move: chess::Changed,
    expect_board: [[char; 9]; 10],
}

// 定义不同的棋盘状态
#[derive(PartialEq)]
enum ChessboardState {
    Initial,      // 初始状态，没有进行任何分析
    StartPos,     // 初始棋盘状态
    OurTurn,      // 我方行棋
    OpponentTurn, // 对方行棋
    Invalid,      // 无效状态
}

// 分析上下文，保存分析状态和共享数据
struct AnalysisContext {
    app: AppHandle,
    window: ListenWindow,
    last_board: [[char; 9]; 10],
    expect_move: chess::Changed,
    expect_board: [[char; 9]; 10],
    invalid_change_count: usize,
}

unsafe impl Send for AnalysisContext {}
unsafe impl Sync for AnalysisContext {}

impl AnalysisContext {
    fn new(app: AppHandle, window: ListenWindow) -> Self {
        Self {
            app,
            // state_for_thread: state,
            window,
            last_board: [[' '; 9]; 10],
            expect_move: chess::Changed::default(),
            expect_board: [[' '; 9]; 10],
            invalid_change_count: 0,
        }
    }

    fn execute_move(&self, pv: &str, camp: &chess::Camp) {
        #[cfg(target_os = "windows")]
        {
            if pv.len() < 4 {
                return;
            }
            let mut cs = pv.chars();
            let from_x = cs.next().unwrap() as usize - 97; // 'a' is 97
            let from_y = 57 - cs.next().unwrap() as usize; // '9' is 57
            let to_x = cs.next().unwrap() as usize - 97;
            let to_y = 57 - cs.next().unwrap() as usize;

            // 根据阵营（是否黑方）做坐标映射，还原回截图中的实际行列
            let (raw_from_x, raw_from_y) = if camp.is_black() {
                (8 - from_x, 9 - from_y)
            } else {
                (from_x, from_y)
            };

            let (raw_to_x, raw_to_y) = if camp.is_black() {
                (8 - to_x, 9 - to_y)
            } else {
                (to_x, to_y)
            };

            let w_f = self.window.w as f32;
            let h_f = self.window.h as f32;
            if w_f <= 0.0 || h_f <= 0.0 {
                return;
            }

            // 计算相对窗口的坐标
            let from_cx = self.window.x as f32 + (raw_from_x as f32 + 0.5) * (w_f / 9.0);
            let from_cy = self.window.y as f32 + (raw_from_y as f32 + 0.5) * (h_f / 10.0);

            let to_cx = self.window.x as f32 + (raw_to_x as f32 + 0.5) * (w_f / 9.0);
            let to_cy = self.window.y as f32 + (raw_to_y as f32 + 0.5) * (h_f / 10.0);

            let hwnd_val = self.window.id();
            let hwnd = hwnd_val as usize as win32::HWND;
            let mut rect = win32::RECT::default();
            unsafe {
                if win32::IsWindow(hwnd) != 0 {
                    win32::GetWindowRect(hwnd, &mut rect);
                    
                    let screen_from_x = rect.left + from_cx as i32;
                    let screen_from_y = rect.top + from_cy as i32;
                    let screen_to_x = rect.left + to_cx as i32;
                    let screen_to_y = rect.top + to_cy as i32;

                    info!("自动下棋执行: {} -> {} (屏幕位置: {},{} -> {},{})", 
                          &pv[..2], &pv[2..], screen_from_x, screen_from_y, screen_to_x, screen_to_y);

                    // 激活窗口
                    win32::SetForegroundWindow(hwnd);
                    thread::sleep(Duration::from_millis(150));

                    // 点击起点
                    win32::SetCursorPos(screen_from_x, screen_from_y);
                    thread::sleep(Duration::from_millis(150));
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    thread::sleep(Duration::from_millis(100));
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);

                    thread::sleep(Duration::from_millis(300));

                    // 点击终点
                    win32::SetCursorPos(screen_to_x, screen_to_y);
                    thread::sleep(Duration::from_millis(150));
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    thread::sleep(Duration::from_millis(100));
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            info!("当前平台不支持自动下棋着法模拟: {}", pv);
        }
    }

    // 检查是否需要终止分析线程
    fn should_stop(&self) -> bool {
        let state = SHARED_STATE.get().unwrap();
        state.listen_thread.lock().unwrap().is_none()
    }

    // 获取棋盘图像并分析
    fn capture_and_analyze_board(&self) -> Option<(chess::Camp, [[char; 9]; 10])> {
        let image = self.window.capture();
        get_board(image)
    }

    // 确认棋盘状态是否稳定
    fn confirm_board(&self, board: [[char; 9]; 10]) -> bool {
        thread::sleep(Duration::from_millis(100));
        let conf_image = self.window.capture();
        if let Some((_, conf_board)) = get_board(conf_image) {
            return conf_board == board;
        }
        false
    }

    // 分析棋盘并返回结果
    fn analyze_board(&mut self, camp: &chess::Camp, board: [[char; 9]; 10]) -> Option<BoardAnalysisResult> {
        let fen = chess::board_fen(camp, board);
        let config = SHARED_STATE.get().unwrap().config.read().unwrap();
        let state = SHARED_STATE.get().unwrap();
        let mut engine = state.engine.lock().unwrap();
        let result = block_on(engine.search(&fen, &config.engine));
        result.as_ref()?;

        let (expect_move, expect_board) = analyse(&self.app, result.unwrap(), board);

        // 如果开启了自动下棋，则自动执行该着法
        if config.autoplay {
            // 稍作延迟以获得更好的稳定性和观感
            thread::sleep(Duration::from_millis(600));
            let pv = expect_move.from.clone() + &expect_move.to;
            self.execute_move(&pv, camp);
        }

        Some(BoardAnalysisResult { expect_move, expect_board })
    }

    // 更新UI显示
    fn update_ui(&self, camp: &chess::Camp, board: [[char; 9]; 10]) {
        let board_map = chess::board_map(board);
        self.app.emit("mirror", camp.is_black()).unwrap();
        self.app.emit("position", &board_map).unwrap();
    }

    // 处理移动事件
    fn handle_move(&mut self, changed: &chess::Changed) { self.app.emit("move", changed).unwrap(); }

    // 处理错误变化计数
    fn handle_invalid_change(
        &mut self, last_board: [[char; 9]; 10], board: [[char; 9]; 10], camp: &chess::Camp,
    ) -> ChessboardState {
        if self.invalid_change_count < 3 {
            self.invalid_change_count += 1;
            let last_fen = chess::board_fen(camp, last_board);
            let current = chess::board_fen(camp, board);
            debug!("OneChanged last {}", last_fen);
            debug!("OneChanged current {}", current);
            ChessboardState::Invalid
        } else {
            // 如果出现次数超过3次，重置为初始状态
            debug!("OneChanged count=3, reload");
            self.invalid_change_count = 0;
            ChessboardState::Initial
        }
    }
}

pub fn get_board(image: ImageBuffer<Rgba<u8>, Vec<u8>>) -> Option<(chess::Camp, [[char; 9]; 10])> {
    let data = predict(image).unwrap();
    if let Ok((camp, mut board)) = common::detections_to_board(&data) {
        chess::board_fix(&camp, &mut board);
        Some((camp, board))
    } else {
        None
    }
}

pub fn analyse(app: &AppHandle, mut result: QueryResult, board: [[char; 9]; 10]) -> (chess::Changed, [[char; 9]; 10]) {
    // 引擎结果翻译为中文
    let best_pv = result.pvs.first().unwrap();
    let best_move = chess::board_move_chinese(board, best_pv);
    let expect_board = chess::board_move(board, best_pv);
    let expect_move = chess::Changed::from_pv(best_pv, board);

    let mut tmp_board = expect_board;
    result.moves.push(best_move);
    for pv in result.pvs.iter().skip(1).take(3) {
        let mv = chess::board_move_chinese(tmp_board, pv);
        result.moves.push(mv);
        tmp_board = chess::board_move(tmp_board, pv);
    }
    // 把结果发送给前端
    info!("分析结果 {:?}", result);
    app.emit("analyse", result).unwrap();

    // 返回一个预期move和预期board
    (expect_move, expect_board)
}

// 处理循环逻辑的主函数
fn process_analysis_loop(mut context: AnalysisContext) {
    let mut current_state = ChessboardState::Initial;

    loop {
        // 检查是否需要停止监听
        if context.should_stop() {
            debug!("listen stopped");
            break;
        }

        // 获取等待间隔
        let interval = SHARED_STATE.get().unwrap().config.read().unwrap().timer_interval;
        thread::sleep(Duration::from_millis(interval));

        // 捕获并分析棋盘
        let board_result = context.capture_and_analyze_board();
        if board_result.is_none() {
            continue;
        }

        let (camp, board) = board_result.unwrap();
        trace!("{:?} {:?}", camp, board);

        // 根据不同状态处理棋盘
        current_state = match current_state {
            ChessboardState::Initial => {
                // 初始状态，做第一次分析
                debug!("首次启动，立即分析");

                // 设置前端棋盘
                context.update_ui(&camp, board);

                // 分析当前棋盘
                if let Some(result) = context.analyze_board(&camp, board) {
                    context.expect_move = result.expect_move;
                    context.expect_board = result.expect_board;
                }

                context.last_board = board;

                // 如果是初始棋盘，进入初始状态，否则进入一般状态
                if chess::startpos(board) {
                    ChessboardState::StartPos
                } else if camp.eq(&chess::Camp::Red) {
                    ChessboardState::OurTurn
                } else {
                    ChessboardState::OpponentTurn
                }
            }

            ChessboardState::StartPos => {
                // 判断棋盘是否仍然是初始棋盘
                if !chess::startpos(board) {
                    // 不再是初始棋盘，处理正常的棋局变化
                    if board == context.last_board {
                        ChessboardState::StartPos // 没有变化
                    } else {
                        // 有变化，更新UI并分析
                        let (changed, board_state) = chess::board_diff(context.last_board, board);

                        match board_state {
                            chess::BoardChangeState::Move => {
                                context.last_board = board;
                                context.handle_move(&changed);

                                if camp.eq(&changed.camp) {
                                    // 我方移动
                                    ChessboardState::OurTurn
                                } else {
                                    // 对方移动，需要分析
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                }
                            }
                            chess::BoardChangeState::One => {
                                context.handle_invalid_change(context.last_board, board, &camp)
                            }
                            chess::BoardChangeState::Unknown => {
                                debug!("棋局变化未知，重置上下文");
                                context.update_ui(&camp, board);
                                context.last_board = board;
                                ChessboardState::Initial
                            }
                        }
                    }
                } else if chess::Camp::Red.eq(&camp) {
                    // 仍然是初始棋盘，且我方先手
                    if context.last_board == board {
                        // 防止重复分析
                        ChessboardState::StartPos
                    } else {
                        // 设置前端棋盘
                        context.last_board = board;
                        context.update_ui(&camp, board);

                        // 调用引擎查询
                        if let Some(result) = context.analyze_board(&camp, board) {
                            context.expect_move = result.expect_move;
                            context.expect_board = result.expect_board;
                        }

                        ChessboardState::OurTurn
                    }
                } else {
                    // 对方先手，跳过分析
                    debug!("对方先手，跳过分析");
                    context.last_board = board;
                    context.update_ui(&camp, board);
                    ChessboardState::OpponentTurn
                }
            }

            ChessboardState::OurTurn | ChessboardState::OpponentTurn => {
                // 判断棋盘是否未发生变化
                if board == context.last_board {
                    debug!("棋盘未发生变化，跳过分析");
                    current_state // 保持当前状态
                } else if board == context.expect_board {
                    // 符合预期棋盘，跳过分析
                    debug!("棋盘为预期棋盘，跳过分析");
                    let expect_move = context.expect_move.clone();
                    let expect_board = context.expect_board;
                    context.last_board = expect_board;
                    context.handle_move(&expect_move);

                    // 更换下一个行动方
                    if current_state == ChessboardState::OurTurn {
                        ChessboardState::OpponentTurn
                    } else {
                        ChessboardState::OurTurn
                    }
                } else {
                    // 确认棋盘变化是否稳定
                    if !context.confirm_board(board) {
                        debug!("棋盘延迟确认失败");
                        let confirm_interval = SHARED_STATE.get().unwrap().config.read().unwrap().confirm_interval;
                        thread::sleep(Duration::from_millis(confirm_interval));
                        current_state // 保持当前状态
                    } else if !chess::board_check(board) {
                        // 检测棋盘是否有效
                        let debug_fen = chess::board_fen(&camp, board);
                        debug!("棋盘识别无效: {}", debug_fen);
                        current_state // 保持当前状态
                    } else {
                        // 处理正常棋盘变化
                        let (changed, board_state) = chess::board_diff(context.last_board, board);

                        match board_state {
                            chess::BoardChangeState::Move => {
                                context.last_board = board;
                                context.handle_move(&changed);

                                if camp.eq(&changed.camp) {
                                    // 我方移动，跳过分析
                                    debug!("我方移动, {} -> {}, 跳过分析", changed.from, changed.to);
                                    ChessboardState::OurTurn
                                } else {
                                    // 对方移动，需要分析
                                    debug!("对方移动, {} -> {}, 需要分析", changed.from, changed.to);
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                }
                            }
                            chess::BoardChangeState::One => {
                                context.handle_invalid_change(context.last_board, board, &camp)
                            }
                            chess::BoardChangeState::Unknown => {
                                debug!("棋局变化未知，重置上下文");
                                context.update_ui(&camp, board);
                                context.last_board = board;
                                ChessboardState::Initial
                            }
                        }
                    }
                }
            }

            ChessboardState::Invalid => {
                // 复位到初始状态，等待下一次有效的变化
                ChessboardState::Initial
            }
        };
    }
}

// 初始化Tauri的command处理
#[tauri::command]
pub async fn start_listen(app: AppHandle, target: Window) -> Result<(), String> {
    trace!("start_listen");
    if SHARED_STATE.get().unwrap().listen_thread.try_lock().is_err() {
        error!("current listen thread is running, please stop it first");
        return Err("已经在监听中".to_string());
    }

    // 初始化监听窗口模块
    let mut window = ListenWindow::new(&target, IMAGE_WIDTH, IMAGE_HEIGHT).unwrap(); // 创建窗口实例
    let image = window.capture();

    let image_h = image.height();
    let image_w = image.width();

    let detections = predict(image).unwrap();

    match common::detections_bound(image_w, image_h, &detections) {
        Ok((x, y, w, h)) => {
            window.set_sub_bound(x, y, w, h); // 设置窗口边界
        }
        Err(e) => {
            return Err(e); // 未识别到棋盘
        }
    }

    // 创建分析上下文
    let context = AnalysisContext::new(app.clone(), window);

    // 启动后台线程进行截图和处理
    let listen_thread = thread::spawn(move || {
        trace!("into thread");
        process_analysis_loop(context);
    });

    SHARED_STATE.get().unwrap().listen_thread.lock().unwrap().replace(listen_thread);

    Ok(())
}

#[tauri::command]
pub fn stop_listen() {
    info!("stop listen");
    let shared_state = SHARED_STATE.get().unwrap();
    if let Ok(mut state) = shared_state.listen_thread.lock()
        && let Some(listen_thread) = state.take() {
            // 释放锁，停止后台线程
            debug!("释放锁，停止后台线程");
            drop(state);
            listen_thread.join().unwrap();
        }
    debug!("stoped");
}
