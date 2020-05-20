
fn indicatif() {
    use rand::seq::SliceRandom;
    use rand::Rng;
    use std::cmp::min;
    use std::thread;
    use std::time::{Duration, Instant};

    use console::{style, Emoji};
    use indicatif::{HumanDuration, MultiProgress, ProgressBar, ProgressStyle};

    

    let started = Instant::now();
    println!(
        "{} Start Downloading Sequence",
        style("[*/*]").bold().dim(),
    );

    let m = MultiProgress::new();
    let sty = ProgressStyle::default_bar()
        .template("{prefix:.bold.dim} {spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg} ({eta})")
        .progress_chars("#>-");

    let pb = m.add(ProgressBar::new(128));
    pb.set_style(sty.clone());
    pb.set_prefix(&format!("[{}/1]", 1));
    let _ = thread::spawn(move || {
        for i in 0..128 {
            pb.set_message(&format!("item #{}", i + 1));
            pb.inc(1);
            thread::sleep(Duration::from_millis(15));
        }
        pb.finish_with_message("done");
    });

    let pb = m.add(ProgressBar::new(128));
    pb.set_style(sty.clone());
    let _ = thread::spawn(move || {
        for k in 0..3 {
            pb.set_prefix(&format!("[{}/3]", k + 1));
            pb.set_position(0);
            for i in 0..128 {
                pb.set_message(&format!("item #{}", i + 1));
                pb.inc(1);
                thread::sleep(Duration::from_millis(8));
            }
        }
        pb.finish_with_message("done");
    });


    let pb = m.add(ProgressBar::new(1024));
    pb.set_style(sty.clone());
    pb.set_prefix(&format!("[{}/?]", 1));
    let _ = thread::spawn(move || {
        for i in 0..1024 {
            pb.set_message(&format!("item #{}", i + 1));
            pb.inc(1);
            thread::sleep(Duration::from_millis(2));
        }
        pb.finish_with_message("done");
    });

    //m.join_and_clear().unwrap();
    m.join().unwrap();


    println!("Done in {}", HumanDuration(started.elapsed()));
}