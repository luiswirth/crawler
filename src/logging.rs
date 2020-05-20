pub fn setup() -> std::result::Result<(), fern::InitError> {
    use fern::{
        colors::{Color, ColoredLevelConfig},
        Dispatch,
    };

    let file_dispatch = Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                chrono::Local::now().format("%H:%M:%S"),
                record.target(),
                record.level(),
                message
            ))
        })
        .chain(fern::log_file(format!(
            "logs/{}.log",
            chrono::Local::now().format("%Y-%m-%d_%H%M%S")
        ))?);

    let colors = ColoredLevelConfig::new()
        .trace(Color::Blue)
        .info(Color::Green)
        .warn(Color::Magenta)
        .error(Color::Red);

    // TODO: nicer formatting for stdout ex: `=>` or `>` in color
    let term_dispatch = Dispatch::new()
        .format(move |out, message, record| {
            let mut target = record.target().to_string();
            if target == "crawler" {
                target = "".to_string();
            } else {
                target = format!("[{}]", target);
            }
            out.finish(format_args!(
                "{}[{}] {}",
                target,
                colors.color(record.level()),
                message
            ))
        })
        .chain(std::io::stdout());

    Dispatch::new()
        .level(log::LevelFilter::Info)
        .level_for("surf::middleware::logger::native", log::LevelFilter::Error)
        .chain(term_dispatch)
        .chain(file_dispatch)
        .apply()?;

    Ok(())
}
