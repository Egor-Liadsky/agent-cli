use termimad::{crossterm::style::Color, MadSkin};

pub fn agent_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    skin.set_headers_fg(Color::AnsiValue(214)); // оранжевый
    skin.bold.set_fg(Color::Yellow);
    skin.italic.set_fg(Color::Magenta);
    skin.inline_code.set_fg(Color::Cyan);
    skin.inline_code.set_bg(Color::AnsiValue(236));
    skin.code_block.set_fg(Color::Cyan);
    skin.code_block.set_bg(Color::AnsiValue(236));
    skin.quote_mark.set_fg(Color::Green);
    skin.bullet.set_fg(Color::Green);
    skin.scrollbar.thumb.set_fg(Color::Cyan);
    skin
}
