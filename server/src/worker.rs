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
    #[derive(Debug, Clone, Copy, Default)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    pub struct HWND__(c_void);
    pub type HWND = *mut HWND__;

    unsafe extern "system" {
        pub fn GetWindowRect(hwnd: HWND, lpRect: *mut RECT) -> i32;
        pub fn SetForegroundWindow(hwnd: HWND) -> i32;
        pub fn SetCursorPos(x: i32, y: i32) -> i32;
        pub fn GetCursorPos(lpPoint: *mut POINT) -> i32;
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
    has_pending_click: bool,
    last_action_time: Option<std::time::Instant>,
    last_auto_match_time: Option<std::time::Instant>,
    last_recalibrate_time: Option<std::time::Instant>,
    last_board_change_time: std::time::Instant,
    last_invalid_click_time: Option<std::time::Instant>,
    current_camp: chess::Camp,
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
            has_pending_click: false,
            last_action_time: None,
            last_auto_match_time: None,
            last_recalibrate_time: None,
            last_board_change_time: std::time::Instant::now(),
            last_invalid_click_time: None,
            current_camp: chess::Camp::None,
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

                    // 记录原先的鼠标位置，防止影响用户操作
                    let mut original_pos = win32::POINT::default();
                    win32::GetCursorPos(&mut original_pos);

                    // 激活窗口
                    win32::SetForegroundWindow(hwnd);
                    thread::sleep(Duration::from_millis(150));

                    // 点击起点：在此期间强制把鼠标坐标定位到起点
                    win32::SetCursorPos(screen_from_x, screen_from_y);
                    thread::sleep(Duration::from_millis(150));
                    win32::SetCursorPos(screen_from_x, screen_from_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    thread::sleep(Duration::from_millis(100));
                    win32::SetCursorPos(screen_from_x, screen_from_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);

                    thread::sleep(Duration::from_millis(300));

                    // 点击终点：在此期间强制把鼠标坐标定位到终点
                    win32::SetCursorPos(screen_to_x, screen_to_y);
                    thread::sleep(Duration::from_millis(150));
                    win32::SetCursorPos(screen_to_x, screen_to_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    thread::sleep(Duration::from_millis(100));
                    win32::SetCursorPos(screen_to_x, screen_to_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);

                    // 恢复鼠标指针到用户原本的位置
                    thread::sleep(Duration::from_millis(50));
                    win32::SetCursorPos(original_pos.x, original_pos.y);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            info!("当前平台不支持自动下棋着法模拟: {}", pv);
        }
    }

    fn click_screen_pos(&self, client_x: u32, client_y: u32) {
        #[cfg(target_os = "windows")]
        {
            let hwnd_val = self.window.id();
            let hwnd = hwnd_val as usize as win32::HWND;
            let mut rect = win32::RECT::default();
            unsafe {
                if win32::IsWindow(hwnd) != 0 {
                    win32::GetWindowRect(hwnd, &mut rect);
                    
                    let screen_x = rect.left + client_x as i32;
                    let screen_y = rect.top + client_y as i32;

                    // 记录原先的鼠标位置，防止影响用户操作
                    let mut original_pos = win32::POINT::default();
                    win32::GetCursorPos(&mut original_pos);

                    // 激活窗口
                    win32::SetForegroundWindow(hwnd);
                    thread::sleep(Duration::from_millis(150));

                    // 点击该位置
                    win32::SetCursorPos(screen_x, screen_y);
                    thread::sleep(Duration::from_millis(100));
                    win32::SetCursorPos(screen_x, screen_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                    thread::sleep(Duration::from_millis(100));
                    win32::SetCursorPos(screen_x, screen_y);
                    win32::mouse_event(win32::MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);

                    // 恢复鼠标位置
                    thread::sleep(Duration::from_millis(50));
                    win32::SetCursorPos(original_pos.x, original_pos.y);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            info!("当前平台不支持窗口坐标模拟点击: {}, {}", client_x, client_y);
        }
    }

    // 尝试检测并点击“再来一局”或段位提升返回按钮
    fn try_auto_match(&mut self, current_state: &mut ChessboardState) -> bool {
        let auto_match = SHARED_STATE.get().unwrap().config.read().unwrap().auto_match;
        if !auto_match {
            return false;
        }

        let now = std::time::Instant::now();
        let should_check = match self.last_auto_match_time {
            None => true,
            Some(last_time) => now.duration_since(last_time) > Duration::from_secs(5),
        };

        if should_check {
            let image = self.window.capture_full();

            // 1. 优先检测“段位提升”界面的返回箭头
            if let Some((btn_x, btn_y)) = check_rank_up_back_btn(&image) {
                info!("[自动匹配] 检测到段位提升返回箭头，执行自动点击！");
                self.click_screen_pos(btn_x, btn_y);
                self.last_auto_match_time = Some(now);
                return true;
            }

            // 2. 然后检测“再来一局”按钮
            if let Some((btn_x, btn_y)) = check_play_again_btn(&image) {
                info!("[自动匹配] 检测到“再来一局”按钮，执行自动点击！");
                self.click_screen_pos(btn_x, btn_y);
                self.last_auto_match_time = Some(now);
                
                // 游戏已结束，清除挂起点击，防止它继续重试刚才的着法
                self.has_pending_click = false;
                self.last_action_time = None;
                
                *current_state = ChessboardState::Initial;
                return true;
            }
        }
        false
    }

    // 动态校准/重新定位棋盘边界，以应对模拟器移动、缩放等变化
    fn recalibrate_board_bound(&mut self) -> bool {
        let now = std::time::Instant::now();
        if let Some(last_time) = self.last_recalibrate_time {
            if now.duration_since(last_time) < Duration::from_secs(2) {
                return false;
            }
        }
        self.last_recalibrate_time = Some(now);

        let image = self.window.capture_full();
        let _ = image.save("d:\\PythonSpace\\chessboard\\artifacts\\recalibrate_capture.png");
        let image_h = image.height();
        let image_w = image.width();
        
        if let Ok(detections) = predict(image) {
            info!("动态校准检测到的框: {:?}", detections);
            if let Ok((x, y, w, h)) = common::detections_bound(image_w, image_h, &detections) {
                info!("动态重新校准棋盘边界成功: x={}, y={}, w={}, h={}", x, y, w, h);
                self.window.set_sub_bound(x, y, w, h);
                return true;
            }
        }
        false
    }

    // 检查是否需要终止分析线程
    fn should_stop(&self) -> bool {
        let state = SHARED_STATE.get().unwrap();
        state.listen_thread.lock().unwrap().is_none()
    }

    // 获取棋盘图像并分析
    fn capture_and_analyze_board(&self) -> Option<(chess::Camp, [[char; 9]; 10])> {
        let image = self.window.capture();
        let require_board_outline = self.window.w == 0;
        get_board(image, require_board_outline, &self.current_camp)
    }

    // 确认棋盘状态是否稳定
    fn confirm_board(&self, board: [[char; 9]; 10]) -> bool {
        thread::sleep(Duration::from_millis(100));
        let conf_image = self.window.capture();
        let require_board_outline = self.window.w == 0;
        if let Some((_, conf_board)) = get_board(conf_image, require_board_outline, &self.current_camp) {
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
            self.has_pending_click = true;
            self.last_action_time = Some(std::time::Instant::now());
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

    // 棋盘识别无效或被遮挡时，每隔 6 秒自动点击一次屏幕x正中间、y三分之二处，以驱散可能的干扰弹窗
    fn handle_invalid_board_click(&mut self) {
        let now = std::time::Instant::now();
        let should_click = match self.last_invalid_click_time {
            None => true,
            Some(last_time) => now.duration_since(last_time) > Duration::from_secs(6),
        };
        if should_click {
            let (width, height) = self.window.full_size();
            let click_x = width / 2;
            let click_y = height * 2 / 3;
            if click_x > 0 && click_y > 0 {
                info!("[自愈点击] 棋盘识别无效/被遮挡，尝试点击屏幕中下部（x中间, y 2/3处）以清除弹窗 ({}, {})", click_x, click_y);
                self.click_screen_pos(click_x, click_y);
                self.last_invalid_click_time = Some(now);
            }
        }
    }
}

pub fn get_board(
    image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    require_board_outline: bool,
    tracked_camp: &chess::Camp,
) -> Option<(chess::Camp, [[char; 9]; 10])> {
    let data = predict(image).unwrap();
    if let Ok((mut camp, mut board)) = common::detections_to_board(&data, require_board_outline) {
        if camp == chess::Camp::None && *tracked_camp != chess::Camp::None {
            camp = tracked_camp.clone();
        }
        chess::board_fix(&camp, &mut board);
        Some((camp, board))
    } else {
        None
    }
}

fn scan_green_button_in_region(
    image: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    width: u32,
    _height: u32,
    y_start: u32,
    y_end: u32,
) -> Option<(u32, u32)> {
    let x_start = (width as f32 * 0.45) as u32;
    let x_end = (width as f32 * 0.55) as u32;

    let mut green_pixels = 0;
    let mut sum_x = 0u64;
    let mut sum_y = 0u64;

    for y in y_start..y_end {
        for x in x_start..x_end {
            let pixel = image.get_pixel(x, y);
            let r = pixel.0[0];
            let g = pixel.0[1];
            let b = pixel.0[2];

            if g > 65 && g > r.saturating_add(26) && g > b.saturating_add(45) {
                green_pixels += 1;
                sum_x += x as u64;
                sum_y += y as u64;
            }
        }
    }

    let total_scanned = (x_end - x_start) * (y_end - y_start);
    let threshold = (total_scanned / 20).max(100);

    if green_pixels > threshold {
        let click_x = (sum_x / green_pixels as u64) as u32;
        let click_y = (sum_y / green_pixels as u64) as u32;
        Some((click_x, click_y))
    } else {
        None
    }
}

pub fn check_play_again_btn(image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Option<(u32, u32)> {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return None;
    }
    
    // 区域一：中间偏下位置（弹窗的“确定”按钮），范围在 72% - 78%
    let region_b_y_start = (height as f32 * 0.72) as u32;
    let region_b_y_end = (height as f32 * 0.78) as u32;
    if let Some(pos) = scan_green_button_in_region(image, width, height, region_b_y_start, region_b_y_end) {
        info!("检测到弹窗的“确定”按钮！");
        return Some(pos);
    }

    // 区域二：底部位置（“再来一局”按钮），范围在 89% - 93%
    let region_a_y_start = (height as f32 * 0.89) as u32;
    let region_a_y_end = (height as f32 * 0.93) as u32;
    if let Some(pos) = scan_green_button_in_region(image, width, height, region_a_y_start, region_a_y_end) {
        info!("检测到“再来一局”按钮！");
        return Some(pos);
    }

    None
}

pub fn check_rank_up_back_btn(image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Option<(u32, u32)> {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return None;
    }

    // 段位提升界面的返回箭头通常在左下角
    // x 范围在 [0.02 * width, 0.15 * width]，y 范围在 [0.90 * height, 0.98 * height]
    let x_start = (width as f32 * 0.02) as u32;
    let x_end = (width as f32 * 0.15) as u32;
    let y_start = (height as f32 * 0.90) as u32;
    let y_end = (height as f32 * 0.98) as u32;

    let mut matched_arrow_pixels = 0;
    let mut matched_blue_bg_pixels = 0;
    let mut total_bg_pixels = 0;
    let mut sum_x = 0u64;
    let mut sum_y = 0u64;

    for y in y_start..y_end {
        for x in x_start..x_end {
            if x >= width || y >= height {
                continue;
            }
            let pixel = image.get_pixel(x, y);
            let r = pixel.0[0];
            let g = pixel.0[1];
            let b = pixel.0[2];

            // 匹配金色返回箭头的颜色特征
            let is_arrow = r > 115 && g > 105 && r > g.saturating_sub(10) && r > b.saturating_sub(15) && b < r.saturating_add(15);
            if is_arrow {
                matched_arrow_pixels += 1;
                sum_x += x as u64;
                sum_y += y as u64;
            } else {
                total_bg_pixels += 1;
                // 匹配深蓝色背景特征
                if b > r.saturating_add(40) && b > g.saturating_add(40) {
                    matched_blue_bg_pixels += 1;
                }
            }
        }
    }

    // 计算区域内除箭头外的背景像素中深蓝色的比例
    let blue_bg_ratio = if total_bg_pixels > 0 {
        matched_blue_bg_pixels as f32 / total_bg_pixels as f32
    } else {
        0.0
    };

    // 只有当检测到足够数量的金色箭头像素（至少 200 个）且背景确实是深蓝色（比例大于 80%）时，才判定为检测到返回按钮
    if matched_arrow_pixels > 200 && blue_bg_ratio > 0.80 {
        let click_x = (sum_x / matched_arrow_pixels) as u32;
        let click_y = (sum_y / matched_arrow_pixels) as u32;
        Some((click_x, click_y))
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

        // 1. 如果用户关闭了“自动下棋”，清除任何挂起的点击状态，立即停止自动点击棋子
        let autoplay = SHARED_STATE.get().unwrap().config.read().unwrap().autoplay;
        if !autoplay {
            context.has_pending_click = false;
            context.last_action_time = None;
        }

        // 2. 如果开启了自动匹配，且处于初始状态或长时间无动作（防止对局中误检测），每隔 6 秒进行一次例行“再来一局”检测
        let auto_match = SHARED_STATE.get().unwrap().config.read().unwrap().auto_match;
        if auto_match {
            let now = std::time::Instant::now();
            let should_check = match context.last_auto_match_time {
                None => true,
                Some(last_time) => now.duration_since(last_time) > Duration::from_secs(6),
            };
            
            // 对局正在进行（当前非 Initial 状态且最近 12 秒内有棋盘变化），则跳过例行检测以防误触头像
            let last_change_elapsed = context.last_board_change_time.elapsed() > Duration::from_secs(12);
            let is_idle_or_initial = current_state == ChessboardState::Initial || last_change_elapsed;

            if should_check && is_idle_or_initial {
                let image = context.window.capture_full();
                
                // 1. 优先检测并点击段位提升返回箭头
                if let Some((btn_x, btn_y)) = check_rank_up_back_btn(&image) {
                    info!("[自动匹配] 例行检测中发现段位提升返回箭头，执行自动点击！");
                    context.click_screen_pos(btn_x, btn_y);
                    context.last_auto_match_time = Some(now);
                    continue;
                }

                // 2. 然后检测并点击“再来一局”按钮
                if let Some((btn_x, btn_y)) = check_play_again_btn(&image) {
                    info!("[自动匹配] 例行检测中发现“再来一局”按钮，执行自动点击！");
                    context.click_screen_pos(btn_x, btn_y);
                    context.last_auto_match_time = Some(now);
                    
                    // 游戏已结束，清除下棋点击及重试计时器
                    context.has_pending_click = false;
                    context.last_action_time = None;
                    
                    current_state = ChessboardState::Initial;
                    continue;
                }
                context.last_auto_match_time = Some(now); // 更新时间以开始下一个 6 秒冷却周期
            }
        }

        if current_state == ChessboardState::Initial {
            context.current_camp = chess::Camp::None;
        }

        // 捕获并分析棋盘
        let mut board_result = context.capture_and_analyze_board();
        if board_result.is_none() {
            // 棋盘未识别到，尝试自动校准边界并重试
            if context.recalibrate_board_bound() {
                board_result = context.capture_and_analyze_board();
            }
        }

        if board_result.is_none() {
            context.handle_invalid_board_click();
            context.try_auto_match(&mut current_state);
            continue;
        }

        let (mut camp, mut board) = board_result.unwrap();
        if camp != chess::Camp::None {
            context.current_camp = camp.clone();
        }
        trace!("{:?} {:?}", camp, board);

        // 如果棋盘被识别为无效棋盘（例如弹窗遮挡、尺寸或位置发生改变）
        if !chess::board_check(board) {
            // 尝试自动校准边界并重试
            if context.recalibrate_board_bound() {
                if let Some((new_camp, new_board)) = context.capture_and_analyze_board() {
                    if chess::board_check(new_board) {
                        camp = new_camp;
                        board = new_board;
                    }
                }
            }
        }

        // 如果依然是无效棋盘，再进行常规兜底处理
        if !chess::board_check(board) {
            let debug_fen = chess::board_fen(&camp, board);
            debug!("棋盘识别无效: {}", debug_fen);
            context.handle_invalid_board_click();
            context.try_auto_match(&mut current_state);
            continue;
        }

        // 如果棋盘发生改变，更新最后变化时间以避免对局期间误判“再来一局”
        if board != context.last_board {
            context.last_board_change_time = std::time::Instant::now();
        }

        // 检测自动下棋点击是否超时未生效，并在超时后重新发送点击事件以避免死锁
        if context.has_pending_click {
            if board == context.expect_board {
                // 已生效，清除挂起状态
                context.has_pending_click = false;
                context.last_action_time = None;
            } else if let Some(last_time) = context.last_action_time {
                if last_time.elapsed() > Duration::from_secs(3) {
                    // 如果开启了自动匹配，点击未生效很有可能是因为对局已经结束，棋子无法移动，
                    // 此时我们在重试前先检测并点击“再来一局”
                    let auto_match = SHARED_STATE.get().unwrap().config.read().unwrap().auto_match;
                    if auto_match {
                        let image = context.window.capture_full();
                        
                        // 1. 优先检测并点击段位提升返回箭头
                        if let Some((btn_x, btn_y)) = check_rank_up_back_btn(&image) {
                            info!("[自动下棋重试] 棋子未移动，但检测到段位提升返回箭头，停止重发着法，执行返回点击！");
                            context.click_screen_pos(btn_x, btn_y);
                            context.last_auto_match_time = Some(std::time::Instant::now());
                            context.has_pending_click = false;
                            context.last_action_time = None;
                            continue;
                        }

                        // 2. 然后检测并点击“再来一局”按钮
                        if let Some((btn_x, btn_y)) = check_play_again_btn(&image) {
                            info!("[自动下棋重试] 棋子未移动，但检测到“再来一局”按钮，停止重发着法，执行匹配点击！");
                            context.click_screen_pos(btn_x, btn_y);
                            context.last_auto_match_time = Some(std::time::Instant::now());
                            context.has_pending_click = false;
                            context.last_action_time = None;
                            current_state = ChessboardState::Initial;
                            continue;
                        }
                    }

                    info!("自动下棋未检测到预期棋盘，尝试重新执行着法: {} -> {}", context.expect_move.from, context.expect_move.to);
                    let pv = context.expect_move.from.clone() + &context.expect_move.to;
                    context.execute_move(&pv, &camp);
                    context.last_action_time = Some(std::time::Instant::now());
                }
            }
        }

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
                    context.has_pending_click = false;
                    context.last_action_time = None;
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

    let _ = std::fs::create_dir_all("d:\\PythonSpace\\chessboard\\artifacts");
    let _ = image.save("d:\\PythonSpace\\chessboard\\artifacts\\startup_capture.png");

    let image_h = image.height();
    let image_w = image.width();

    let detections = predict(image).unwrap();
    info!("启动时检测到的框: {:?}", detections);

    let (x, y, w, h) = match common::detections_bound(image_w, image_h, &detections) {
        Ok((x, y, w, h)) => (x, y, w, h),
        Err(_) => {
            info!("首次启动未识别到棋盘（可能被结算弹窗遮挡），使用窗口全屏作为默认边界，依靠后续的自动匹配与自愈机制校准");
            (0, 0, image_w, image_h)
        }
    };
    window.set_sub_bound(x, y, w, h); // 设置窗口边界

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
