use std::path::PathBuf;
use std::sync::Arc;

use egui::{Context, FontData, FontFamily};
use egui::FontDefinitions;

/// 字体族下拉的第一项：沿用自动挑选（首选 PingFang SC，找不到再按兜底表往下走）
pub const AUTO: &str = "自动";

/// 字号下拉。走 egui 的 zoom_factor（= 系统 DPI × 这个倍数），文字、控件、卡片一起放大，
/// 不会只把字撑大而把排好的版式挤坏
pub const SCALES: &[(&str, f32)] = &[("小", 0.9), ("标准", 1.0), ("大", 1.15), ("特大", 1.3)];

/// 默认字号名
pub const SCALE_STANDARD: &str = "标准";

/// 字号名 → zoom_factor。认不出来的名字（手改配置写错）按「标准」处理
pub fn scale_factor(name: &str) -> f32 {
    SCALES.iter().find(|(n, _)| *n == name).map(|(_, factor)| *factor).unwrap_or(1.0)
}

/// 一个候选中文字体族。`keyword` 用来判断本机装没装（文件名小写后包含即算），
/// `regular` / `bold` 是 Windows 字体目录里实际存在的文件名，按优先级排——
/// 同一族往往躺着 Light / Heavy 好几个字重，只靠关键字会挑错，所以点名。
/// 留空表示没有需要点名的文件，交给关键字 + 字重词判断（如苹方各版本文件名写法不一）。
struct Family {
    name: &'static str,
    keyword: &'static str,
    regular: &'static [&'static str],
    bold: &'static [&'static str],
}

/// 下拉里可选的字体族。列在这里的必须是 Windows 上真能装到的，否则设置页永远见不到
const FAMILIES: &[Family] = &[
    Family { name: "苹方", keyword: "pingfang", regular: &[], bold: &[] },
    Family {
        name: "微软雅黑",
        keyword: "msyh",
        regular: &["msyh.ttc", "msyh.ttf"],
        bold: &["msyhbd.ttc", "msyhbd.ttf"],
    },
    Family {
        name: "等线",
        keyword: "deng",
        regular: &["deng.ttf", "dengx.ttf"],
        bold: &["dengb.ttf"],
    },
    Family { name: "黑体", keyword: "simhei", regular: &["simhei.ttf"], bold: &[] },
    Family { name: "宋体", keyword: "simsun", regular: &["simsun.ttc"], bold: &[] },
    Family { name: "楷体", keyword: "simkai", regular: &["simkai.ttf"], bold: &[] },
    Family { name: "思源黑体", keyword: "sourcehansans", regular: &[], bold: &[] },
    Family { name: "霞鹜文楷", keyword: "lxgwwenkai", regular: &[], bold: &[] },
];

/// 首选字体：各台机器上 PingFang SC 的文件名写法不一（可能带空格、大小写不同），按关键字匹配。
const PREFERRED: &str = "pingfang";

/// 首选字体里算粗体的字重（按文件名关键字判断）。
const BOLD_WORDS: &[&str] = &["bold", "semibold", "medium", "black"];

/// 指定族没有粗体字面时，兜底使用的中文粗体字体文件名。
const BOLD_FONTS: &[&str] = &[
    "PingFangSC-Semibold.ttf",
    "PingFang SC Semibold.ttf",
    "msyhbd.ttc",
    "Dengb.ttf",
    "simhei.ttf",
];

/// 找不到首选字体时按文件名兜底的中文字体。
const CJK_FONTS: &[&str] = &[
    "PingFang SC.ttf",
    "PingFangSC-Regular.ttf",
    "PingFang-SC-Regular.ttf",
    "msyh.ttc",
    "msyh.ttf",
    "Deng.ttf",
    "simhei.ttf",
    "Microsoft YaHei.ttf",
    "simkai.ttf",
    "STXIHEI.TTF",
    "simsun.ttc",
    "NotoSansSC-Regular.otf",
    "NotoSansCJKsc-Regular.otf",
];

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for name in ["WINDIR", "SystemRoot"] {
        if let Some(root) = std::env::var_os(name) {
            dirs.push(PathBuf::from(root).join("Fonts"));
        }
    }
    dirs.push(PathBuf::from("C:\\Windows\\Fonts"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
    }
    dirs
}

/// 字体目录里所有能当字体用的文件（一次列完，选族、判断装没装都在这份上找）
fn font_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in font_dirs() {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().to_lowercase();
            if [".ttf", ".otf", ".ttc"].iter().any(|ext| name.ends_with(ext)) {
                out.push(item.path());
            }
        }
    }
    out
}

fn lower_name(path: &PathBuf) -> String {
    path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// 按给定的文件名顺序（不区分大小写）在这台机器上找第一个存在的
fn first_named(files: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|want| {
        files.iter().find(|path| lower_name(path) == *want).cloned()
    })
}

/// 命中关键字的文件，按字重排序（Regular 优先）；`want_bold` 决定取粗体那一批还是正文那一批
fn keyword_hits(files: &[PathBuf], keyword: &str, want_bold: bool) -> Vec<PathBuf> {
    let mut hits: Vec<(u8, usize, PathBuf)> = files
        .iter()
        .enumerate()
        .filter(|(_, path)| lower_name(path).contains(keyword))
        .filter(|(_, path)| {
            BOLD_WORDS.iter().any(|word| lower_name(path).contains(word)) == want_bold
        })
        .map(|(order, path)| {
            (if want_bold { 0 } else { weight_rank(&lower_name(path)) }, order, path.clone())
        })
        .collect();
    hits.sort();
    hits.into_iter().map(|(_, _, path)| path).collect()
}

/// 在某个族里找正文 / 粗体用的文件：先点名，再按关键字捞
fn find(files: &[PathBuf], family: &Family, want_bold: bool) -> Option<PathBuf> {
    let named = if want_bold { family.bold } else { family.regular };
    first_named(files, named).or_else(|| keyword_hits(files, family.keyword, want_bold).into_iter().next())
}

/// 这个字体文件属于哪个族（按文件名关键字认，认不出来算「自动」）。
/// 用在启动提示上：配置里选的族本机没有时会退回别的文件，这里就说清真正在用的那个族。
pub fn family_of(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    FAMILIES
        .iter()
        .find(|family| lower.contains(family.keyword))
        .map(|family| family.name.to_owned())
        .unwrap_or_else(|| AUTO.to_owned())
}

/// 本机装了哪些中文字体族：下拉里只列真找得到的，「自动」永远在第一位
pub fn available() -> Vec<String> {
    let files = font_files();
    let mut out = vec![AUTO.to_owned()];
    out.extend(
        FAMILIES
            .iter()
            .filter(|family| find(&files, family, false).is_some())
            .map(|family| family.name.to_owned()),
    );
    out
}

fn weight_rank(name: &str) -> u8 {
    if name.contains("regular") {
        0
    } else if ["thin", "light", "medium", "bold", "semibold", "black"]
        .iter()
        .any(|word| name.contains(word))
    {
        2
    } else {
        1
    }
}

/// 启动提示与设置页底部那行字体说明：如实写出真正在用的是哪个族、哪个文件。
/// `loaded` 是 `install` 返回的字体文件路径。
pub fn note_for(choice: &str, loaded: Option<String>) -> String {
    let Some(path) = loaded else {
        return "警告：未找到系统中文字体，中文可能显示为方块".to_owned();
    };
    let file = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(path);
    let used = family_of(&file);
    match choice {
        AUTO => format!("界面字体：自动 · {used}（{file}）"),
        want if want == used => format!("界面字体：{used}（{file}）"),
        want => format!("界面字体：{want}（本机没有，实际用 {used} · {file}）"),
    }
}

/// 载入选定的中文字体并注册给 egui，返回真正用到的字体文件名。
///
/// `family` 取 `available()` 里的族名；填「自动」或者这台机器上没有这个族，都退回原来的
/// 挑选顺序（首选苹方 → 兜底表），所以配置文件里留一个别的机器上的族名也不会把界面弄成方块。
///
/// 这里除了正文与等宽字族，还必须给 `FontFamily::Name("bold")` 建立绑定：
/// epaint 默认只有 Proportional / Monospace 两个字族，排版一个没绑定过的字族会直接 panic
/// （`FontFamily::Name("bold") is not bound to any fonts`），
/// 而 panic 发生在排版过程中，画面会停在最后一帧，看起来就是「按钮能点却没反应」。
pub fn install(ctx: &Context, family: &str) -> Option<String> {
    let files = font_files();
    let chosen = FAMILIES.iter().find(|item| item.name == family);
    // 候选链：选定族的文件排最前，后面始终接上「自动」的原有顺序——
    // 这样选的族这台机器上没有、或者那个文件读不动，也不会把界面弄成方块
    let mut bodies: Vec<PathBuf> = Vec::new();
    let mut bolds: Vec<PathBuf> = Vec::new();
    if let Some(family) = chosen {
        bodies.extend(find(&files, family, false));
        bolds.extend(find(&files, family, true));
    }
    bodies.extend(keyword_hits(&files, PREFERRED, false));
    bodies.extend(CJK_FONTS.iter().filter_map(|name| first_named(&files, &[*name])));
    bolds.extend(keyword_hits(&files, PREFERRED, true));
    bolds.extend(BOLD_FONTS.iter().filter_map(|name| first_named(&files, &[*name])));
    let mut fonts = FontDefinitions::default();
    let mut loaded: Option<String> = None;
    for path in bodies {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        // 字体文件比这还小基本就是坏文件，读进来只会画不出字
        if bytes.len() < 4096 {
            continue;
        }
        let data = FontData::from_owned(bytes).tweak(egui::FontTweak {
            scale: 1.0,
            y_offset_factor: 0.02,
            ..Default::default()
        });
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        loaded = Some(path.to_string_lossy().into_owned());
        break;
    }
    if loaded.is_some() {
        if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
            list.insert(0, "cjk".to_owned());
        }
        if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
            list.push("cjk".to_owned());
        }
    }
    // 粗体字面：先取选定族自己的粗体，再退回系统里现成的中文粗体
    let mut bold: Vec<String> = Vec::new();
    for path in bolds {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if bytes.len() < 4096 {
            continue;
        }
        let data = FontData::from_owned(bytes).tweak(egui::FontTweak {
            scale: 1.0,
            y_offset_factor: 0.02,
            ..Default::default()
        });
        fonts.font_data.insert("cjk-bold".to_owned(), Arc::new(data));
        bold.push("cjk-bold".to_owned());
        break;
    }
    // 兜底：任何情况下 Name("bold") 都至少指向正文用到的字体
    if let Some(list) = fonts.families.get(&FontFamily::Proportional) {
        bold.extend(list.iter().cloned());
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(fonts);
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{FontId, RawInput, RichText};

    /// 下拉里列出来的族，选哪个都得真能把字排出来：坏文件在 epaint 里是 panic，
    /// 表现就是「界面停在最后一帧，按钮能点没反应」。只取前四个族——一个族就是
    /// 十几兆的字体文件，全读一遍太慢，前四个已经覆盖「点名挑文件」和「关键字挑文件」两条路。
    #[test]
    fn every_listed_family_lays_out() {
        let ctx = Context::default();
        for family in available().into_iter().take(4) {
            assert!(install(&ctx, &family).is_some(), "{family} 没装上");
            let mut widths = [0.0; 2];
            let mut output = ctx.run_ui(RawInput::default(), |ui| {
                widths[0] = ui.label("本地差异 7 项").rect.width();
                widths[1] = ui
                    .label(
                        RichText::new("冲突 3 项")
                            .font(FontId { size: 12.0, family: FontFamily::Name("bold".into()) }),
                    )
                    .rect.width();
            });
            // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
            output.textures_delta.clear();
            assert!(widths[0] > 0.0 && widths[1] > 0.0, "{family} 排版失败：{widths:?}");
        }
    }

    /// 配置文件是手改得到的，留一个别的机器上的族名不能把界面弄坏：照「自动」挑
    #[test]
    fn unknown_family_falls_back_to_auto() {
        let ctx = Context::default();
        assert_eq!(install(&ctx, "没这个族"), install(&ctx, AUTO));
    }

    /// 设置页底部那行说明要如实写出真正在用的族：选「自动」要说自动，
    /// 选的族本机没有要说清退回用了什么，一个中文字体都没有时要报警
    #[test]
    fn font_note_tells_which_family_is_actually_used() {
        let msyh = Some(r"C:\Windows\Fonts\msyh.ttc".to_owned());
        assert_eq!(note_for(AUTO, msyh.clone()), "界面字体：自动 · 微软雅黑（msyh.ttc）");
        assert_eq!(note_for("微软雅黑", msyh.clone()), "界面字体：微软雅黑（msyh.ttc）");
        let fallback = note_for("苹方", msyh);
        assert!(fallback.contains("苹方") && fallback.contains("本机没有"), "{fallback}");
        assert!(note_for("微软雅黑", None).contains("方块"));
    }

    /// 字号名要能对上 zoom_factor，认不出来的按「标准」
    #[test]
    fn scale_names_map_to_zoom() {
        assert_eq!(scale_factor("标准"), 1.0);
        assert!(scale_factor("小") < 1.0, "小字号该比标准小");
        assert!(scale_factor("大") > 1.0 && scale_factor("特大") > scale_factor("大"));
        assert_eq!(scale_factor("没这个字号"), 1.0);
    }

    /// epaint 遇到没绑定过的字族会在排版时直接 panic（界面就此停在最后一帧，
    /// 表现就是「按钮看着能点，点了没反应」），所以 Name("bold") 必须始终可用。
    #[test]
    fn bold_family_always_lays_out() {
        let ctx = Context::default();
        install(&ctx, AUTO);
        let mut width = 0.0;
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let bold = FontId {
                size: 12.0,
                family: FontFamily::Name("bold".into()),
            };
            width = ui.label(RichText::new("药品目录 HRP.DRUG").font(bold)).rect.width();
        });
        // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
        output.textures_delta.clear();
        assert!(width > 0.0, "粗体字族没能排出文字");
    }

    /// 正文与等宽字族同样不能因为换字体而被删掉。
    #[test]
    fn body_and_mono_stay_bound() {
        let ctx = Context::default();
        install(&ctx, AUTO);
        let mut widths = [0.0; 2];
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            widths[0] = ui.label("正文").rect.width();
            widths[1] = ui.monospace("mono").rect.width();
        });
        output.textures_delta.clear();
        assert!(widths[0] > 0.0 && widths[1] > 0.0, "{widths:?}");
    }

    /// 界面里用到的特殊符号必须真的有字形，否则会渲染成方块（tofu）。
    /// 教训：✕(U+2715) 在中文字体和 egui 内置字体里都没有，用户看到的就是方块。
    /// 这里逐一检查正文（Proportional）字族：缺一个就在用它的地方换字。
    #[test]
    fn ui_symbols_have_glyphs() {
        let ctx = Context::default();
        install(&ctx, AUTO);
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            // 字体要等第一次 run_ui 排版后才就绪，所以检查放在 UI 闭包里
            let body = FontId::proportional(12.0);
            // 界面字符串里用到的全部特殊符号。曾用过 📂🕘⬆⬇📝✨🔗✖✏，
            // 实测这些 emoji 在中文字体和 egui 内置字体里都没有字形（渲染成方块），
            // 已全部换成下面这些实测有字形的符号或纯文本。
            let symbols = [
                ('×', "AI 日志移除按钮"),
                ('←', "返回目录按钮"),
                ('→', "补充目录按钮 / 日志行"),
                ('↑', "上传按钮 / 新版本提示"),
                ('↓', "更新按钮"),
                ('●', "状态灯"),
                ('✓', "提交成功提示 / relocate 按钮"),
                ('↔', "换行符说明"),
                ('⚖', "绑定对比对象菜单"),
            ];
            ui.ctx().fonts_mut(|fonts| {
                let mut missing: Vec<(char, &str)> = Vec::new();
                for (ch, usage) in symbols {
                    if !fonts.has_glyph(&body, ch) {
                        missing.push((ch, usage));
                    }
                }
                assert!(
                    missing.is_empty(),
                    "这些符号在正文字体里没有字形（会显示成方块）：{missing:?}"
                );
            });
        });
        // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
        output.textures_delta.clear();
    }
}