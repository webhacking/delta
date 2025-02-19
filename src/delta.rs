use std::io::Write;

use ansi_term::Colour::{Blue, Yellow};
use console::strip_ansi_codes;
use unicode_segmentation::UnicodeSegmentation;

use crate::bat::assets::HighlightingAssets;
use crate::cli;
use crate::config::Config;
use crate::draw;
use crate::paint::Painter;
use crate::parse;
use crate::style;

#[derive(Debug, PartialEq)]
pub enum State {
    CommitMeta, // In commit metadata section
    FileMeta,   // In diff metadata section, between (possible) commit metadata and first hunk
    HunkMeta,   // In hunk metadata line
    HunkZero,   // In hunk; unchanged line
    HunkMinus,  // In hunk; removed line
    HunkPlus,   // In hunk; added line
    Unknown,
}

#[derive(Debug, PartialEq)]
pub enum Source {
    GitDiff,     // Coming from a `git diff` command
    DiffUnified, // Coming from a `diff -u` command
    Unknown,
}

impl State {
    fn is_in_hunk(&self) -> bool {
        match *self {
            State::HunkMeta | State::HunkZero | State::HunkMinus | State::HunkPlus => true,
            _ => false,
        }
    }
}

// Possible transitions, with actions on entry:
//
//
// | from \ to   | CommitMeta  | FileMeta    | HunkMeta    | HunkZero    | HunkMinus   | HunkPlus |
// |-------------+-------------+-------------+-------------+-------------+-------------+----------|
// | CommitMeta  | emit        | emit        |             |             |             |          |
// | FileMeta    |             | emit        | emit        |             |             |          |
// | HunkMeta    |             |             |             | emit        | push        | push     |
// | HunkZero    | emit        | emit        | emit        | emit        | push        | push     |
// | HunkMinus   | flush, emit | flush, emit | flush, emit | flush, emit | push        | push     |
// | HunkPlus    | flush, emit | flush, emit | flush, emit | flush, emit | flush, push | push     |

pub fn delta<I>(
    lines: I,
    config: &Config,
    assets: &HighlightingAssets,
    writer: &mut dyn Write,
) -> std::io::Result<()>
where
    I: Iterator<Item = String>,
{
    let mut lines_peekable = lines.peekable();
    let mut painter = Painter::new(writer, config, assets);
    let mut minus_file = "".to_string();
    let mut plus_file;
    let mut state = State::Unknown;
    let source = detect_source(&mut lines_peekable);

    for raw_line in lines_peekable {
        if source == Source::Unknown {
            writeln!(painter.writer, "{}", raw_line)?;
            continue;
        }

        let line = strip_ansi_codes(&raw_line).to_string();
        if line.starts_with("commit ") {
            painter.paint_buffered_lines();
            state = State::CommitMeta;
            if config.opt.commit_style != cli::SectionStyle::Plain {
                painter.emit()?;
                handle_commit_meta_header_line(&mut painter, &raw_line, config)?;
                continue;
            }
        } else if line.starts_with("diff ") {
            painter.paint_buffered_lines();
            state = State::FileMeta;
            painter.set_syntax(parse::get_file_extension_from_diff_line(&line));
        } else if (line.starts_with("--- ") || line.starts_with("rename from "))
            && config.opt.file_style != cli::SectionStyle::Plain
        {
            if source == Source::DiffUnified {
                state = State::FileMeta;
                painter.set_syntax(parse::get_file_extension_from_marker_line(&line));
            }
            minus_file = parse::get_file_path_from_file_meta_line(&line, source == Source::GitDiff);
        } else if (line.starts_with("+++ ") || line.starts_with("rename to "))
            && config.opt.file_style != cli::SectionStyle::Plain
        {
            plus_file = parse::get_file_path_from_file_meta_line(&line, source == Source::GitDiff);
            painter.emit()?;
            handle_file_meta_header_line(
                &mut painter,
                &minus_file,
                &plus_file,
                config,
                source == Source::DiffUnified,
            )?;
        } else if line.starts_with("@@ ") {
            state = State::HunkMeta;
            painter.set_highlighter();
            if config.opt.hunk_style != cli::SectionStyle::Plain {
                painter.emit()?;
                handle_hunk_meta_line(&mut painter, &line, config)?;
                continue;
            }
        } else if source == Source::DiffUnified && line.starts_with("Only in ")
            || line.starts_with("Submodule ")
        {
            // Additional FileMeta cases:
            //
            // 1. When comparing directories with diff -u, if filenames match between the
            //    directories, the files themselves will be compared. However, if an equivalent
            //    filename is not present, diff outputs a single line (Only in...) starting
            //    indicating that the file is present in only one of the directories.
            //
            // 2. Git diff emits lines describing submodule state such as "Submodule x/y/z contains
            //    untracked content"
            //
            // See https://github.com/dandavison/delta/issues/60#issuecomment-557485242 for a
            // proposal for more robust parsing logic.

            state = State::FileMeta;
            painter.paint_buffered_lines();
            if config.opt.file_style != cli::SectionStyle::Plain {
                painter.emit()?;
                handle_generic_file_meta_header_line(&mut painter, &raw_line, config)?;
                continue;
            }
        } else if state.is_in_hunk() {
            state = handle_hunk_line(&mut painter, &line, state, config);
            painter.emit()?;
            continue;
        }

        if state == State::FileMeta && config.opt.file_style != cli::SectionStyle::Plain {
            // The file metadata section is 4 lines. Skip them under non-plain file-styles.
            continue;
        } else {
            painter.emit()?;
            writeln!(painter.writer, "{}", raw_line)?;
        }
    }

    painter.paint_buffered_lines();
    painter.emit()?;
    Ok(())
}

/// Try to detect what is producing the input for delta by examining the first line
///
/// Currently can detect:
/// * git diff
/// * diff -u
///
/// If the source is not recognized, delta will print the unaltered
/// input back out
fn detect_source<I>(lines: &mut std::iter::Peekable<I>) -> Source
where
    I: Iterator<Item = String>,
{
    lines.peek().map_or(Source::Unknown, |first_line| {
        let line = strip_ansi_codes(&first_line).to_string();

        if line.starts_with("commit ") || line.starts_with("diff --git ") {
            Source::GitDiff
        } else if line.starts_with("diff -u ")
            || line.starts_with("diff -U")
            || line.starts_with("--- ")
        {
            Source::DiffUnified
        } else {
            Source::Unknown
        }
    })
}

fn handle_commit_meta_header_line(
    painter: &mut Painter,
    line: &str,
    config: &Config,
) -> std::io::Result<()> {
    let draw_fn = match config.opt.commit_style {
        cli::SectionStyle::Box => draw::write_boxed_with_line,
        cli::SectionStyle::Underline => draw::write_underlined,
        cli::SectionStyle::Plain => panic!(),
    };
    draw_fn(
        painter.writer,
        line,
        config.terminal_width,
        Yellow.normal(),
        true,
    )?;
    Ok(())
}

/// Construct file change line from minus and plus file and write with FileMeta styling.
fn handle_file_meta_header_line(
    painter: &mut Painter,
    minus_file: &str,
    plus_file: &str,
    config: &Config,
    comparing: bool,
) -> std::io::Result<()> {
    let line = parse::get_file_change_description_from_file_paths(minus_file, plus_file, comparing);
    handle_generic_file_meta_header_line(painter, &line, config)
}

/// Write `line` with FileMeta styling.
fn handle_generic_file_meta_header_line(
    painter: &mut Painter,
    line: &str,
    config: &Config,
) -> std::io::Result<()> {
    let draw_fn = match config.opt.file_style {
        cli::SectionStyle::Box => draw::write_boxed_with_line,
        cli::SectionStyle::Underline => draw::write_underlined,
        cli::SectionStyle::Plain => panic!(),
    };
    let ansi_style = Blue.normal();
    writeln!(painter.writer)?;
    draw_fn(
        painter.writer,
        &ansi_style.paint(line),
        config.terminal_width,
        ansi_style,
        false,
    )?;
    Ok(())
}

fn handle_hunk_meta_line(
    painter: &mut Painter,
    line: &str,
    config: &Config,
) -> std::io::Result<()> {
    let draw_fn = match config.opt.hunk_style {
        cli::SectionStyle::Box => draw::write_boxed,
        cli::SectionStyle::Underline => draw::write_underlined,
        cli::SectionStyle::Plain => panic!(),
    };
    let ansi_style = Blue.normal();
    let (raw_code_fragment, line_number) = parse::parse_hunk_metadata(&line);
    let code_fragment = prepare(raw_code_fragment, config.tab_width, false);
    if !code_fragment.is_empty() {
        let syntax_style_sections = Painter::get_line_syntax_style_sections(
            &code_fragment,
            &mut painter.highlighter,
            &painter.config,
            true,
        );
        Painter::paint_lines(
            &mut painter.output_buffer,
            vec![syntax_style_sections],
            vec![vec![(
                style::NO_BACKGROUND_COLOR_STYLE_MODIFIER,
                &code_fragment,
            )]],
            config,
            style::NO_BACKGROUND_COLOR_STYLE_MODIFIER,
            false,
        );
        painter.output_buffer.pop(); // trim newline
        draw_fn(
            painter.writer,
            &painter.output_buffer,
            config.terminal_width,
            ansi_style,
            false,
        )?;
        painter.output_buffer.clear();
    }
    writeln!(painter.writer, "\n{}", ansi_style.paint(line_number))?;
    Ok(())
}

/// Handle a hunk line, i.e. a minus line, a plus line, or an unchanged line.
// In the case of a minus or plus line, we store the line in a
// buffer. When we exit the changed region we process the collected
// minus and plus lines jointly, in order to paint detailed
// highlighting according to inferred edit operations. In the case of
// an unchanged line, we paint it immediately.
fn handle_hunk_line(painter: &mut Painter, line: &str, state: State, config: &Config) -> State {
    // Don't let the line buffers become arbitrarily large -- if we
    // were to allow that, then for a large deleted/added file we
    // would process the entire file before painting anything.
    if painter.minus_lines.len() > config.max_buffered_lines
        || painter.plus_lines.len() > config.max_buffered_lines
    {
        painter.paint_buffered_lines();
    }
    match line.chars().next() {
        Some('-') => {
            if state == State::HunkPlus {
                painter.paint_buffered_lines();
            }
            painter
                .minus_lines
                .push(prepare(&line, config.tab_width, true));
            State::HunkMinus
        }
        Some('+') => {
            painter
                .plus_lines
                .push(prepare(&line, config.tab_width, true));
            State::HunkPlus
        }
        _ => {
            painter.paint_buffered_lines();
            let line = prepare(&line, config.tab_width, true);
            let syntax_style_sections = Painter::get_line_syntax_style_sections(
                &line,
                &mut painter.highlighter,
                &painter.config,
                true,
            );
            Painter::paint_lines(
                &mut painter.output_buffer,
                vec![syntax_style_sections],
                vec![vec![(style::NO_BACKGROUND_COLOR_STYLE_MODIFIER, &line)]],
                config,
                style::NO_BACKGROUND_COLOR_STYLE_MODIFIER,
                true,
            );
            State::HunkZero
        }
    }
}

/// Replace initial -/+ character with ' ', expand tabs as spaces, and optionally terminate with
/// newline.
// Terminating with newline character is necessary for many of the sublime syntax definitions to
// highlight correctly.
// See https://docs.rs/syntect/3.2.0/syntect/parsing/struct.SyntaxSetBuilder.html#method.add_from_folder
fn prepare(line: &str, tab_width: usize, append_newline: bool) -> String {
    let terminator = if append_newline { "\n" } else { "" };
    if !line.is_empty() {
        let mut line = line.graphemes(true);

        // The first column contains a -/+/space character, added by git. We skip it here and insert
        // a replacement space when formatting the line below.
        line.next();

        // Expand tabs as spaces.
        // tab_width = 0 is documented to mean do not replace tabs.
        let output_line = if tab_width > 0 {
            let tab_replacement = " ".repeat(tab_width);
            line.map(|s| if s == "\t" { &tab_replacement } else { s })
                .collect::<String>()
        } else {
            line.collect::<String>()
        };
        format!(" {}{}", output_line, terminator)
    } else {
        terminator.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::strip_ansi_codes;
    use std::env;
    use syntect::highlighting::StyleModifier;

    use crate::paint;

    #[test]
    fn test_added_file() {
        let options = get_command_line_options();
        let output = strip_ansi_codes(&run_delta(ADDED_FILE_INPUT, &options)).to_string();
        assert!(output.contains("\nadded: a.py\n"));
        if false {
            // TODO: hline width
            assert_eq!(output, ADDED_FILE_EXPECTED_OUTPUT);
        }
    }

    #[test]
    fn test_renamed_file() {
        let options = get_command_line_options();
        let output = strip_ansi_codes(&run_delta(RENAMED_FILE_INPUT, &options)).to_string();
        assert!(output.contains("\nrenamed: a.py ⟶   b.py\n"));
    }

    #[test]
    fn test_recognized_file_type() {
        // In addition to the background color, the code has language syntax highlighting.
        let options = get_command_line_options();
        let input = ADDED_FILE_INPUT;
        let output = get_line_of_code_from_delta(&input, &options);
        assert_has_color_other_than_plus_color(&output, &options);
    }

    #[test]
    fn test_unrecognized_file_type_with_theme() {
        // In addition to the background color, the code has the foreground color using the default
        // .txt syntax under the theme.
        let options = get_command_line_options();
        let input = ADDED_FILE_INPUT.replace("a.py", "a");
        let output = get_line_of_code_from_delta(&input, &options);
        assert_has_color_other_than_plus_color(&output, &options);
    }

    #[test]
    fn test_unrecognized_file_type_no_theme() {
        // The code has the background color only. (Since there is no theme, the code has no
        // foreground ansi color codes.)
        let mut options = get_command_line_options();
        options.theme = Some("none".to_string());
        let input = ADDED_FILE_INPUT.replace("a.py", "a");
        let output = get_line_of_code_from_delta(&input, &options);
        assert_has_plus_color_only(&output, &options);
    }

    #[test]
    fn test_theme_selection() {
        enum Mode {
            Light,
            Dark,
        };
        let assets = HighlightingAssets::new();
        for (
            theme_option,
            bat_theme_env_var,
            mode_option, // (--light, --dark)
            expected_theme,
            expected_mode,
        ) in vec![
            (None, "", None, style::DEFAULT_DARK_THEME, Mode::Dark),
            (Some("GitHub".to_string()), "", None, "GitHub", Mode::Light),
            (
                Some("GitHub".to_string()),
                "1337",
                None,
                "GitHub",
                Mode::Light,
            ),
            (None, "1337", None, "1337", Mode::Dark),
            (
                None,
                "<not set>",
                None,
                style::DEFAULT_DARK_THEME,
                Mode::Dark,
            ),
            (
                None,
                "",
                Some(Mode::Light),
                style::DEFAULT_LIGHT_THEME,
                Mode::Light,
            ),
            (
                None,
                "",
                Some(Mode::Dark),
                style::DEFAULT_DARK_THEME,
                Mode::Dark,
            ),
            (
                None,
                "<@@@@@>",
                Some(Mode::Light),
                style::DEFAULT_LIGHT_THEME,
                Mode::Light,
            ),
            (None, "1337", Some(Mode::Light), "1337", Mode::Light),
            (Some("none".to_string()), "", None, "none", Mode::Dark),
            (
                Some("None".to_string()),
                "",
                Some(Mode::Light),
                "None",
                Mode::Light,
            ),
        ] {
            if bat_theme_env_var == "<not set>" {
                env::remove_var("BAT_THEME")
            } else {
                env::set_var("BAT_THEME", bat_theme_env_var);
            }
            let mut options = get_command_line_options();
            options.theme = theme_option;
            match mode_option {
                Some(Mode::Light) => {
                    options.light = true;
                    options.dark = false;
                }
                Some(Mode::Dark) => {
                    options.light = false;
                    options.dark = true;
                }
                None => {
                    options.light = false;
                    options.dark = false;
                }
            }
            let config = cli::process_command_line_arguments(&assets, &options);
            assert_eq!(config.theme_name, expected_theme);
            if style::is_no_syntax_highlighting_theme_name(expected_theme) {
                assert!(config.theme.is_none())
            } else {
                assert_eq!(config.theme.unwrap().name.as_ref().unwrap(), expected_theme);
            }
            assert_eq!(
                config.minus_style_modifier.background.unwrap(),
                match expected_mode {
                    Mode::Light => style::LIGHT_THEME_MINUS_COLOR,
                    Mode::Dark => style::DARK_THEME_MINUS_COLOR,
                }
            );
            assert_eq!(
                config.minus_emph_style_modifier.background.unwrap(),
                match expected_mode {
                    Mode::Light => style::LIGHT_THEME_MINUS_EMPH_COLOR,
                    Mode::Dark => style::DARK_THEME_MINUS_EMPH_COLOR,
                }
            );
            assert_eq!(
                config.plus_style_modifier.background.unwrap(),
                match expected_mode {
                    Mode::Light => style::LIGHT_THEME_PLUS_COLOR,
                    Mode::Dark => style::DARK_THEME_PLUS_COLOR,
                }
            );
            assert_eq!(
                config.plus_emph_style_modifier.background.unwrap(),
                match expected_mode {
                    Mode::Light => style::LIGHT_THEME_PLUS_EMPH_COLOR,
                    Mode::Dark => style::DARK_THEME_PLUS_EMPH_COLOR,
                }
            );
        }
    }

    fn assert_has_color_other_than_plus_color(string: &str, options: &cli::Opt) {
        let (string_without_any_color, string_with_plus_color_only) =
            get_color_variants(string, &options);
        assert_ne!(string, string_without_any_color);
        assert_ne!(string, string_with_plus_color_only);
    }

    fn assert_has_plus_color_only(string: &str, options: &cli::Opt) {
        let (string_without_any_color, string_with_plus_color_only) =
            get_color_variants(string, &options);
        assert_ne!(string, string_without_any_color);
        assert_eq!(string, string_with_plus_color_only);
    }

    fn get_color_variants(string: &str, options: &cli::Opt) -> (String, String) {
        let assets = HighlightingAssets::new();
        let config = cli::process_command_line_arguments(&assets, &options);

        let string_without_any_color = strip_ansi_codes(string).to_string();
        let string_with_plus_color_only = paint_text(
            &string_without_any_color,
            config.plus_style_modifier,
            &config,
        );
        (string_without_any_color, string_with_plus_color_only)
    }

    fn paint_text(input: &str, style_modifier: StyleModifier, config: &Config) -> String {
        let mut output = String::new();
        let style = config.no_style.apply(style_modifier);
        paint::paint_text(&input, style, &mut output).unwrap();
        output
    }

    fn get_line_of_code_from_delta(input: &str, options: &cli::Opt) -> String {
        let output = run_delta(&input, &options);
        let line_of_code = output.lines().nth(12).unwrap();
        assert!(strip_ansi_codes(line_of_code) == " class X:");
        line_of_code.to_string()
    }

    fn run_delta(input: &str, options: &cli::Opt) -> String {
        let mut writer: Vec<u8> = Vec::new();

        let assets = HighlightingAssets::new();
        let config = cli::process_command_line_arguments(&assets, &options);

        delta(
            input.split("\n").map(String::from),
            &config,
            &assets,
            &mut writer,
        )
        .unwrap();
        String::from_utf8(writer).unwrap()
    }

    fn get_command_line_options() -> cli::Opt {
        cli::Opt {
            light: false,
            dark: false,
            minus_color: None,
            minus_emph_color: None,
            plus_color: None,
            plus_emph_color: None,
            theme: None,
            highlight_removed: false,
            commit_style: cli::SectionStyle::Plain,
            file_style: cli::SectionStyle::Underline,
            hunk_style: cli::SectionStyle::Box,
            width: Some("variable".to_string()),
            tab_width: 4,
            show_background_colors: false,
            list_languages: false,
            list_themes: false,
            compare_themes: false,
            max_line_distance: 0.3,
        }
    }

    #[test]
    fn test_diff_unified_two_files() {
        let options = get_command_line_options();
        let output = strip_ansi_codes(&run_delta(DIFF_UNIFIED_TWO_FILES, &options)).to_string();
        let mut lines = output.split('\n');

        // Header
        assert_eq!(lines.nth(1).unwrap(), "comparing: one.rs ⟶   src/two.rs");
        // Line
        assert_eq!(lines.nth(2).unwrap(), "5");
        // Change
        assert_eq!(lines.nth(2).unwrap(), " println!(\"Hello ruster\");");
        // Next chunk
        assert_eq!(lines.nth(2).unwrap(), "43");
        // Unchanged in second chunk
        assert_eq!(lines.nth(2).unwrap(), " Unchanged");
    }

    #[test]
    fn test_diff_unified_two_directories() {
        let options = get_command_line_options();
        let output =
            strip_ansi_codes(&run_delta(DIFF_UNIFIED_TWO_DIRECTORIES, &options)).to_string();
        let mut lines = output.split('\n');

        // Header
        assert_eq!(
            lines.nth(1).unwrap(),
            "comparing: a/different ⟶   b/different"
        );
        // Line number
        assert_eq!(lines.nth(2).unwrap(), "1");
        // Change
        assert_eq!(lines.nth(2).unwrap(), " This is different from b");
        // File uniqueness
        assert_eq!(lines.nth(2).unwrap(), "Only in a/: just_a");
        // FileMeta divider
        assert!(lines.next().unwrap().starts_with("───────"));
        // Next hunk
        assert_eq!(
            lines.nth(4).unwrap(),
            "comparing: a/more_difference ⟶   b/more_difference"
        );
    }

    #[test]
    fn test_delta_ignores_non_diff_input() {
        let options = get_command_line_options();
        let output = strip_ansi_codes(&run_delta(NOT_A_DIFF_OUTPUT, &options)).to_string();
        assert_eq!(output, NOT_A_DIFF_OUTPUT.to_owned() + "\n");
    }

    #[test]
    fn test_submodule_contains_untracked_content() {
        let options = get_command_line_options();
        let output = strip_ansi_codes(&run_delta(
            SUBMODULE_CONTAINS_UNTRACKED_CONTENT_INPUT,
            &options,
        ))
        .to_string();
        assert!(output.contains("\nSubmodule x/y/z contains untracked content\n"));
    }

    const ADDED_FILE_INPUT: &str = "\
commit d28dc1ac57e53432567ec5bf19ad49ff90f0f7a5
Author: Dan Davison <dandavison7@gmail.com>
Date:   Thu Jul 11 10:41:11 2019 -0400

    .

diff --git a/a.py b/a.py
new file mode 100644
index 0000000..8c55b7d
--- /dev/null
+++ b/a.py
@@ -0,0 +1,3 @@
+# hello
+class X:
+    pass";

    const ADDED_FILE_EXPECTED_OUTPUT: &str = "\
commit d28dc1ac57e53432567ec5bf19ad49ff90f0f7a5
Author: Dan Davison <dandavison7@gmail.com>
Date:   Thu Jul 11 10:41:11 2019 -0400

    .

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
added: a.py
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
────────────────────────────────────────────────────────────────────────────────

────────────────────────────────────────────────────────────────────────────────
 # hello
 class X:
     pass
";

    const RENAMED_FILE_INPUT: &str = "\
commit 1281650789680f1009dfff2497d5ccfbe7b96526
Author: Dan Davison <dandavison7@gmail.com>
Date:   Wed Jul 17 20:40:23 2019 -0400

    rename

diff --git a/a.py b/b.py
similarity index 100%
rename from a.py
rename to b.py
";

    const DIFF_UNIFIED_TWO_FILES: &str = "\
--- one.rs	2019-11-20 06:16:08.000000000 +0100
+++ src/two.rs	2019-11-18 18:41:16.000000000 +0100
@@ -5,3 +5,3 @@
 println!(\"Hello world\");
-println!(\"Hello rust\");
+println!(\"Hello ruster\");

@@ -43,6 +43,6 @@
 // Some more changes
-Change one
 Unchanged
+Change two
 Unchanged
-Change three
+Change four
 Unchanged
";

    const DIFF_UNIFIED_TWO_DIRECTORIES: &str = "\
diff -u a/different b/different
--- a/different	2019-11-20 06:47:56.000000000 +0100
+++ b/different	2019-11-20 06:47:56.000000000 +0100
@@ -1,3 +1,3 @@
 A simple file for testing
 the diff command in unified mode
-This is different from b
+This is different from a
Only in a/: just_a
Only in b/: just_b
--- a/more_difference	2019-11-20 06:47:56.000000000 +0100
+++ b/more_difference	2019-11-20 06:47:56.000000000 +0100
@@ -1,3 +1,3 @@
 Another different file
 with a name that start with 'm' making it come after the 'Only in'
-This is different from b
+This is different from a
";

    const NOT_A_DIFF_OUTPUT: &str = "\
Hello world
This is a regular file that contains:
--- some/file/here 06:47:56.000000000 +0100
+++ some/file/there 06:47:56.000000000 +0100
 Some text here
-Some text with a minus
+Some text with a plus
";

    const SUBMODULE_CONTAINS_UNTRACKED_CONTENT_INPUT: &str = "\
--- a
+++ b
@@ -2,3 +2,4 @@
 x
 y
 z
-a
+b
 z
 y
 x
Submodule x/y/z contains untracked content
";
}
