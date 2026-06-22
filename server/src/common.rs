use tracing::trace;

use crate::chess;
use crate::yolo;

// detections_bound 获取截图的边界
pub fn detections_bound(
    origin_width: u32, origin_height: u32, detections: &[yolo::Detection],
) -> Result<(u32, u32, u32, u32), String> {
    // 方式一：尝试直接找到棋盘边界（label == '0'）
    if let Some(board_det) = detections.iter().find(|d| d.label == '0') {
        // 计算模型图到原图的缩放
        let scale_x = origin_width as f32 / yolo::IMAGE_WIDTH as f32;
        let scale_y = origin_height as f32 / yolo::IMAGE_HEIGHT as f32;

        // 模型坐标 → 原图坐标
        let bx0 = (board_det.x0 * scale_x).max(0.0);
        let by0 = (board_det.y0 * scale_y).max(0.0);
        let bx1 = (board_det.x1 * scale_x).min(origin_width as f32);
        let by1 = (board_det.y1 * scale_y).min(origin_height as f32);

        // 计算原图下的“半格”尺寸
        let board_w = bx1 - bx0;
        let board_h = by1 - by0;
        let half_cell_x = board_w / 8.0 / 2.0;
        let half_cell_y = board_h / 9.0 / 2.0;

        // 计算裁剪框左上
        let crop_x = (bx0 - half_cell_x).max(0.0) as u32;
        let crop_y = (by0 - half_cell_y).max(0.0) as u32;

        // 计算裁剪框右下，在原图范围内
        let x1p = (bx1 + half_cell_x).min(origin_width as f32);
        let y1p = (by1 + half_cell_y).min(origin_height as f32);

        // 宽高 = 右下 - 左上
        let width = (x1p - crop_x as f32) as u32;
        let height = (y1p - crop_y as f32) as u32;

        return Ok((crop_x, crop_y, width, height));
    }

    // 方式二：棋盘边界标签 '0' 识别失败时的自愈兜底机制（从高置信度的棋子分布中反推棋盘边界）
    let pieces: Vec<&yolo::Detection> = detections.iter().filter(|d| d.label != '0').collect();
    if pieces.len() >= 4 {
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        let mut min_y = f32::MAX;
        let mut max_y = f32::MIN;

        for p in &pieces {
            let cx = (p.x0 + p.x1) / 2.0;
            let cy = (p.y0 + p.y1) / 2.0;
            if cx < min_x { min_x = cx; }
            if cx > max_x { max_x = cx; }
            if cy < min_y { min_y = cy; }
            if cy > max_y { max_y = cy; }
        }

        // 标准棋盘横向 9 条线（8 个间距），纵向 10 条线（9 个间距）
        let cell_w = (max_x - min_x) / 8.0;
        let cell_h = (max_y - min_y) / 9.0;

        // 缩放系数
        let scale_x = origin_width as f32 / yolo::IMAGE_WIDTH as f32;
        let scale_y = origin_height as f32 / yolo::IMAGE_HEIGHT as f32;

        let scale_cell_w = cell_w * scale_x;
        let scale_cell_h = cell_h * scale_y;

        // 外推半格距离得到裁剪边界
        let crop_x = ((min_x * scale_x) - scale_cell_w * 0.5).max(0.0) as u32;
        let crop_y = ((min_y * scale_y) - scale_cell_h * 0.5).max(0.0) as u32;

        let x1p = ((max_x * scale_x) + scale_cell_w * 0.5).min(origin_width as f32);
        let y1p = ((max_y * scale_y) + scale_cell_h * 0.5).min(origin_height as f32);

        let width = (x1p - crop_x as f32) as u32;
        let height = (y1p - crop_y as f32) as u32;

        if width > 100 && height > 100 {
            tracing::info!("通过棋子分布估算棋盘边界成功: x={}, y={}, w={}, h={}", crop_x, crop_y, width, height);
            return Ok((crop_x, crop_y, width, height));
        }
    }

    Err("未识别到棋盘边界及足够数量的棋子".to_string())
}

const MODEL_CELL_W: f32 = yolo::IMAGE_WIDTH as f32 / 9.0;
const MODEL_CELL_H: f32 = yolo::IMAGE_HEIGHT as f32 / 10.0;

// detections_to_board 识别结果转换为棋盘结构
pub fn detections_to_board(
    detections: &[yolo::Detection],
    require_board_outline: bool,
) -> Result<(chess::Camp, [[char; 9]; 10]), String> {
    let mut camp = chess::Camp::None;
    let mut board = [[' '; 9]; 10];

    let has_board = if require_board_outline {
        detections.iter().any(|d| d.label == '0')
    } else {
        true
    };

    if has_board {
        for det in detections.iter().filter(|d| d.label != '0') {
            // 中心点
            let cx = (det.x0 + det.x1) / 2.0;
            let cy = (det.y0 + det.y1) / 2.0;
            // 行列：x 轴分成 9 格，y 轴分成 10 格
            let col = (cx / MODEL_CELL_W).floor() as usize; // 0–8
            let row = (cy / MODEL_CELL_H).floor() as usize; // 0–9
            trace!("{} row={} col={}", det.label, row, col);

            // 边界处理
            if !(0..=8).contains(&col) || !(0..=9).contains(&row) {
                continue;
            }

            // 构建board
            board[row][col] = det.label;

            // 判断阵营
            if camp == chess::Camp::None && (3..=5).contains(&col) {
                if row >= 7 {
                    match det.label {
                        'k' => camp = chess::Camp::Black,
                        'K' => camp = chess::Camp::Red,
                        _ => {}
                    }
                } else if row <= 2 {
                    match det.label {
                        'k' => camp = chess::Camp::Red,     // 黑将在上方 -> 我方是红方
                        'K' => camp = chess::Camp::Black,   // 红将在上方 -> 我方是黑方
                        _ => {}
                    }
                }
            }
        }
    } else {
        return Err("not board".to_string());
    }
    Ok((camp, board))
}
