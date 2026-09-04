// 图标光栅化 + .ico 编码。**纯 std, 不碰 windows crate** ——
// 这样 build.rs 能 `#[path]` 直接 include 它在编译期生成 .ico 嵌进 exe,
// tray.rs 拿它的像素做 HICON, main.rs 的 --write-ico 拿它出安装包图标。
// 三处共用一份, 嵌进 exe 的图标、托盘图标、安装包图标不可能画得不一样。

/// (r, g, b)。**故意不用 COLORREF 的 `0x00bbggrr` 排布** —— 那种写法下
/// `0x0000B3FF` 到底是琥珀还是天蓝, 只能靠数字节数, 读代码的人必然会读错。
type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Look {
    Idle,          // 灰
    System,        // 琥珀: 仅防睡
    SystemDisplay, // 蓝: 保持屏幕
    Force,         // 蓝 + 绿点
    Paused,        // 红 + 白斜杠
}

/// 每像素每轴的超采样数, 用于抗锯齿。4x4=16 个样本足够让 32→16 降采样后边缘干净。
const AA: u32 = 4;

fn palette(look: Look) -> Rgb {
    match look {
        Look::Idle => (160, 160, 160),                      // 灰
        Look::System => (255, 179, 0),                      // 琥珀: 仅防睡
        Look::SystemDisplay | Look::Force => (61, 123, 255), // 蓝: 保持屏幕
        Look::Paused => (204, 51, 51),                      // 红
    }
}

/// 把一个状态光栅化成 size×size 的**预乘 alpha** BGRA 缓冲, **自上而下**(row 0 = 顶部)。
///
/// ## 为什么自己光栅化而不用 GDI
///
/// `Ellipse` / `LineTo` 只写 RGB 三个字节, **完全不碰 alpha** —— GDI 对 32bpp DIB
/// 的 alpha 语义是未定义的。缓冲区按"全透明"清零之后, 画上去的像素 alpha 仍是 0,
/// 于是 `CreateIconIndirect` 拿到一张处处全透明的位图, 托盘只能显示空白。
///
/// 实测(独立诊断程序, 逐字复制旧的 GDI 绘制步骤): `Ellipse` + `LineTo` 之后
/// **290 个像素 RGB 非零, 而 alpha 非零的像素是 0 个**; 走完 `CreateIconIndirect`
/// 再 `GetDIBits` 回读, alpha 依然全零。
///
/// Windows 有个"alpha 全零就当没有 alpha 通道、退回用掩码"的兜底, 但各渲染路径
/// 并不一致 —— 这正是"双击一下能看见、过一会儿又变空白"的来源。既然不能依赖它,
/// 就必须自己把 alpha 写对。
///
/// 几何按 32px 设计, 其余尺寸整体缩放 —— .ico 需要多种尺寸, 让 shell 从一张
/// 32px 图降采样到 16px 会糊。
pub fn paint(look: Look, size: usize) -> Vec<u32> {
    let mut px = vec![0u32; size * size];
    let k = size as f32 / 32.0;
    let c = size as f32 / 2.0;

    // 主体圆: 与旧版 Ellipse(6,6)-(26,26) 同几何 —— 圆心 (16,16), 半径 10
    fill_disc(&mut px, size, (c, c), 10.0 * k, palette(look));

    match look {
        // 绿点在右上: 旧版 Ellipse(20,6)-(28,14) —— 圆心 (24,10), 半径 4
        Look::Force => fill_disc(&mut px, size, (24.0 * k, 10.0 * k), 4.0 * k, (0, 224, 0)),
        // 白色斜杠: 旧版 (7,25)->(25,7), 线宽 4
        Look::Paused => stroke_line(
            &mut px,
            size,
            (7.0 * k, 25.0 * k),
            (25.0 * k, 7.0 * k),
            4.0 * k,
            (255, 255, 255),
        ),
        _ => {}
    }
    px
}

fn fill_disc(px: &mut [u32], size: usize, (cx, cy): (f32, f32), radius: f32, color: Rgb) {
    let rr = radius * radius;
    composite(px, size, color, |x, y| {
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= rr
    });
}

/// 圆头线段: 判据是"到线段的距离 ≤ 半宽"
fn stroke_line(px: &mut [u32], size: usize, a: (f32, f32), b: (f32, f32), width: f32, color: Rgb) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let half2 = (width / 2.0) * (width / 2.0);
    composite(px, size, color, |x, y| {
        // 投影到线段并夹到 [0,1], 得到线段上最近的点
        let t = (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0);
        let (ex, ey) = (x - (a.0 + t * dx), y - (a.1 + t * dy));
        ex * ex + ey * ey <= half2
    });
}

/// 超采样求每个像素的覆盖率, 按 source-over 合成上去。
///
/// 覆盖率直接当 alpha 用, 所以边缘是半透明而不是锯齿 —— 32→16 降采样后差别明显。
fn composite<F: Fn(f32, f32) -> bool>(px: &mut [u32], size: usize, color: Rgb, inside: F) {
    let step = 1.0 / AA as f32;
    let total = AA * AA;
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0u32;
            for sy in 0..AA {
                for sx in 0..AA {
                    // 采样点取子格中心, 避免整数边界上的系统性偏移
                    let fx = x as f32 + (sx as f32 + 0.5) * step;
                    let fy = y as f32 + (sy as f32 + 0.5) * step;
                    if inside(fx, fy) {
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                continue;
            }
            let a = (hits * 255 / total) as u8;
            let i = y * size + x;
            px[i] = over(premultiply(color, a), px[i]);
        }
    }
}

/// 直通色 + 覆盖率 -> 预乘 alpha 的 BGRA(u32 里是 `A<<24 | R<<16 | G<<8 | B`,
/// 小端内存序恰好是 B,G,R,A —— 这就是 32bpp DIB 的排布)。
///
/// 32bpp 图标的 alpha 按**预乘**解释。不预乘的话半透明边缘会偏亮, 圆周看着发白。
fn premultiply((r, g, b): Rgb, a: u8) -> u32 {
    let m = |c: u8| (c as u32 * a as u32 + 127) / 255;
    (a as u32) << 24 | m(r) << 16 | m(g) << 8 | m(b)
}

/// 预乘空间里的 source-over: `dst = src + dst * (1 - src_a)`
fn over(src: u32, dst: u32) -> u32 {
    let sa = src >> 24;
    if sa == 255 {
        return src; // 全覆盖, 常见情形直接短路
    }
    let inv = 255 - sa;
    let ch = |shift: u32| {
        let s = (src >> shift) & 0xFF;
        let d = (dst >> shift) & 0xFF;
        (s + (d * inv + 127) / 255).min(255)
    };
    ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

// ───────────────────────────── .ico 编码 ─────────────────────────────

/// 产品图标(蓝色实心圆, 不带角标)的多尺寸 .ico 字节。给安装包、快捷方式、
/// 以及 build.rs 嵌进 exe 用。
///
/// 每一帧存成 PNG(Vista 起 .ico 支持 PNG 帧)。用 BMP 帧要自己拼
/// BITMAPINFOHEADER + AND 掩码, PNG 帧只需把像素喂给编码器。
///
/// **故意不含 256**: 下面的 PNG 编码器不压缩, 一帧 256x256 就是 256 KB,
/// 而这是个纯色圆 —— 256 档只在资源管理器"超大图标"下才用得到, 不值得让
/// 安装包(和 exe 资源段)胖 256 KB。真要加就得先实现 deflate。
pub fn product_ico() -> Vec<u8> {
    // 覆盖实际会被用到的档位: 16(列表/托盘) 32(桌面/快捷方式) 48(图标视图)
    // 64(200% DPI 下的 32 槽位)
    const SIZES: [usize; 4] = [16, 32, 48, 64];

    let frames: Vec<(usize, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| (size, encode_png_rgba(size, &paint(Look::SystemDisplay, size))))
        .collect();

    let mut out: Vec<u8> = Vec::new();
    // ICONDIR: reserved=0, type=1(icon), count
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());

    // ICONDIRENTRY 各 16 字节, 数据紧随其后
    let mut offset = 6 + 16 * frames.len();
    for (size, data) in &frames {
        // 256 在这个字段里必须写 0 —— 它只有一个字节
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(0); // 调色板数
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // 色彩平面
        out.extend_from_slice(&32u16.to_le_bytes()); // 位深
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &frames {
        out.extend_from_slice(data);
    }
    out
}

/// 最小 PNG 编码器: 8 位 RGBA、无过滤、**不压缩**(deflate 的 stored 块)。
///
/// 图标只有几百到几万像素, 不值得引入 flate2 —— 256x256 未压缩也就 256 KB,
/// 而 .ico 只在打包/构建时生成一次。stored 块是合法 deflate, 任何解码器都认。
///
/// 输入是**预乘 alpha** 的 BGRA(光栅化器的输出), PNG 要求非预乘 RGBA, 所以要反乘。
fn encode_png_rgba(size: usize, px: &[u32]) -> Vec<u8> {
    // ── 原始扫描线: 每行前面一个过滤器字节(0 = None) ──
    let mut raw = Vec::with_capacity(size * (1 + size * 4));
    for y in 0..size {
        raw.push(0);
        for x in 0..size {
            let p = px[y * size + x];
            let a = (p >> 24) as u8;
            // 预乘 -> 直通。a=0 时整个像素不可见, 颜色取 0 即可
            let un = |c: u32| -> u8 {
                if a == 0 {
                    0
                } else {
                    ((c * 255 + a as u32 / 2) / a as u32).min(255) as u8
                }
            };
            raw.push(un((p >> 16) & 0xFF)); // R
            raw.push(un((p >> 8) & 0xFF)); // G
            raw.push(un(p & 0xFF)); // B
            raw.push(a);
        }
    }

    let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.push(8); // 位深
    ihdr.push(6); // 颜色类型 6 = RGBA
    ihdr.extend_from_slice(&[0, 0, 0]); // 压缩/过滤/隔行 都用标准值
    png_chunk(&mut png, b"IHDR", &ihdr);

    png_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib 容器 + 全部用 deflate 的 stored(未压缩)块。
/// 每块最多 0xFFFF 字节, LEN 与 ~LEN 都是小端。
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // CMF/FLG: deflate, 32K 窗口, 最快
    let mut chunks = data.chunks(0xFFFF).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(c) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none())); // BFINAL
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            // 0xEDB88320 = 反射的 PNG/zlib 多项式
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOKS: [Look; 5] = [
        Look::Idle,
        Look::System,
        Look::SystemDisplay,
        Look::Force,
        Look::Paused,
    ];

    fn at(px: &[u32], size: usize, x: usize, y: usize) -> u32 {
        px[y * size + x]
    }
    fn alpha(p: u32) -> u32 {
        p >> 24
    }
    /// (r, g, b)
    fn rgb(p: u32) -> (u32, u32, u32) {
        ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF)
    }

    /// **核心回归**: 画出来的像素 alpha 必须非零。
    ///
    /// 旧实现用 GDI 的 `Ellipse` / `LineTo` 绘制, 而它们只写 RGB 不碰 alpha,
    /// 于是整张图 alpha 全零 —— 对 alpha 混合器就是完全透明, 托盘只显示空白。
    #[test]
    fn painted_pixels_have_nonzero_alpha() {
        for look in LOOKS {
            let px = paint(look, 32);
            assert_eq!(alpha(at(&px, 32, 16, 16)), 255, "{:?} 圆心必须完全不透明", look);
            let opaque = px.iter().filter(|p| alpha(**p) == 255).count();
            let visible = px.iter().filter(|p| alpha(**p) > 0).count();
            assert!(opaque > 200, "{:?} 只有 {} 个不透明像素", look, opaque);
            assert!(visible > 300, "{:?} 可见像素只有 {}", look, visible);
        }
    }

    /// 四个角必须透明, 否则图标会变成一个方块
    #[test]
    fn corners_stay_transparent() {
        for look in LOOKS {
            let px = paint(look, 32);
            for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31)] {
                assert_eq!(alpha(at(&px, 32, x, y)), 0, "{:?} 角 ({},{}) 不该有像素", look, x, y);
            }
        }
    }

    /// 预乘不变量: 任何颜色通道都不得超过该像素的 alpha。
    #[test]
    fn premultiplied_invariant_holds() {
        for look in LOOKS {
            for (i, p) in paint(look, 32).iter().enumerate() {
                let a = alpha(*p);
                let (r, g, b) = rgb(*p);
                assert!(r <= a && g <= a && b <= a, "{:?} 像素 {} 未预乘: 0x{:08X}", look, i, p);
            }
        }
    }

    /// 五个外观必须互不相同 —— 否则托盘上区分不出来
    #[test]
    fn every_look_is_visually_distinct() {
        let all: Vec<Vec<u32>> = LOOKS.iter().map(|l| paint(*l, 32)).collect();
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "{:?} 与 {:?} 画出来一样", LOOKS[i], LOOKS[j]);
            }
        }
    }

    /// 主体必须是个居中的圆: 半径 8 处处不透明、半径 12 处处透明。
    #[test]
    fn body_is_a_centred_disc() {
        let px = paint(Look::Idle, 32);
        let c = 16.0;
        for y in 0..32 {
            for x in 0..32 {
                let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
                let d = (dx * dx + dy * dy).sqrt();
                let a = alpha(at(&px, 32, x, y));
                if d <= 8.0 {
                    assert_eq!(a, 255, "({},{}) 距圆心 {:.1} 应实心", x, y, d);
                } else if d >= 12.0 {
                    assert_eq!(a, 0, "({},{}) 距圆心 {:.1} 应在圆外", x, y, d);
                }
            }
        }
    }

    /// 绿点必须在右上。这条钉住"自上而下"约定 —— 约定一改, 拷进 DIB 时的翻转
    /// 就会反, 绿点跑到右下、暂停的斜杠从 / 变成 \。
    #[test]
    fn force_dot_sits_in_the_top_right() {
        let px = paint(Look::Force, 32);
        assert_eq!(alpha(at(&px, 32, 24, 10)), 255, "点心应完全不透明");
        let (r, g, b) = rgb(at(&px, 32, 24, 10));
        assert!(g > 150 && r < 60 && b < 60, "(24,10) 应是绿点, 得到 {},{},{}", r, g, b);

        // 预乘缓冲里边缘像素通道被 alpha 缩过, 直接比色值会误判 —— 用"绿分量占优"判定
        let greenish = |x: usize, y: usize| {
            let p = at(&px, 32, x, y);
            let (r, g, b) = rgb(p);
            alpha(p) > 0 && g > r && g > b
        };
        let top: usize = (0..16).map(|y| (0..32).filter(|x| greenish(*x, y)).count()).sum();
        let bottom: usize = (16..32).map(|y| (0..32).filter(|x| greenish(*x, y)).count()).sum();
        assert!(top > 30, "上半部绿点像素只有 {}", top);
        assert_eq!(bottom, 0, "下半部出现 {} 个绿色像素, 上下判定反了", bottom);
    }

    /// 暂停态白斜杠盖在圆心上(线段 (7,25)-(25,7) 的中点正是 16,16)
    #[test]
    fn paused_has_a_white_slash() {
        let (r, g, b) = rgb(at(&paint(Look::Paused, 32), 32, 16, 16));
        assert!(r > 200 && g > 200 && b > 200, "圆心应被白斜杠覆盖, 得到 {},{},{}", r, g, b);
    }

    /// 任意尺寸都画得出且几何正确(.ico 要 16/32/48/64)
    #[test]
    fn scales_to_every_size() {
        for size in [16usize, 32, 48, 64] {
            let px = paint(Look::SystemDisplay, size);
            assert_eq!(px.len(), size * size);
            // 圆心不透明, 四角透明
            assert_eq!(alpha(at(&px, size, size / 2, size / 2)), 255, "{}px 圆心", size);
            assert_eq!(alpha(at(&px, size, 0, 0)), 0, "{}px 左上角", size);
        }
    }

    // ── PNG / .ico 编码 ──

    /// zlib 流的两个校验必须对: adler32 是 zlib 尾, crc32 是每个 PNG 块尾。
    /// 用已知答案钉住 —— 自己实现的校验和写错很难从"图标看着正常"里发现。
    #[test]
    fn checksums_match_known_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398); // RFC 1950 例子
        assert_eq!(adler32(b""), 1);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082); // PNG 规范: 空 IEND 块的 CRC
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// stored 块头必须是 `BFINAL LEN ~LEN`, 且 LEN 与 ~LEN 互补
    #[test]
    fn stored_deflate_blocks_are_well_formed() {
        let data = vec![0xABu8; 0x1_0000 + 5]; // 跨两个块
        let z = zlib_stored(&data);
        assert_eq!(&z[..2], &[0x78, 0x01], "zlib 头");
        assert_eq!(z[2], 0, "第一块不该是 BFINAL");
        let len1 = u16::from_le_bytes([z[3], z[4]]);
        assert_eq!(len1, 0xFFFF);
        assert_eq!(u16::from_le_bytes([z[5], z[6]]), !len1);
        let p = 7 + 0xFFFF;
        assert_eq!(z[p], 1, "最后一块必须是 BFINAL");
        let len2 = u16::from_le_bytes([z[p + 1], z[p + 2]]);
        assert_eq!(len2, 6);
        assert_eq!(u16::from_le_bytes([z[p + 3], z[p + 4]]), !len2);
        let tail = &z[z.len() - 4..];
        assert_eq!(u32::from_be_bytes(tail.try_into().unwrap()), adler32(&data));
    }

    /// 预乘 -> 直通的反乘必须还原出原色
    #[test]
    fn png_encoding_unpremultiplies_alpha() {
        // 半透明纯红: 预乘后 R = 255 * 128 / 255 = 128
        let png = encode_png_rgba(1, &[(128u32 << 24) | (128 << 16)]);
        let idat = 8 + 25 + 8; // 签名 + IHDR + IDAT 头
        let raw = &png[idat + 7..idat + 7 + 5]; // zlib 头 2 + stored 头 5
        assert_eq!(raw[0], 0, "过滤器 None");
        assert_eq!(raw[1], 255, "R 应还原为 255, 得到 {}", raw[1]);
        assert_eq!((raw[2], raw[3]), (0, 0));
        assert_eq!(raw[4], 128, "alpha 原样保留");

        let clear = encode_png_rgba(1, &[0]); // alpha=0 不能除零
        assert_eq!(&clear[idat + 7..idat + 7 + 5][1..], &[0, 0, 0, 0]);
    }

    /// product_ico 产出的字节至少是个合法 ICONDIR: 头正确、4 帧、偏移不越界。
    /// (它能否被 Windows 真的加载, 由 main.rs 的 written_ico_loads_at_every_size
    /// 走 LoadImageW 验证 —— 那需要 windows crate, 不放这个纯 std 模块里。)
    #[test]
    fn product_ico_has_a_sane_directory() {
        let ico = product_ico();
        assert_eq!(u16::from_le_bytes([ico[0], ico[1]]), 0, "reserved");
        assert_eq!(u16::from_le_bytes([ico[2], ico[3]]), 1, "type=icon");
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        assert_eq!(count, 4);
        for i in 0..count {
            let e = 6 + 16 * i;
            let len = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
            let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
            assert!(off + len <= ico.len(), "帧 {} 偏移越界", i);
            // 每帧都应是 PNG(签名前 4 字节)
            assert_eq!(&ico[off..off + 4], &[0x89, b'P', b'N', b'G'], "帧 {} 不是 PNG", i);
        }
    }
}
