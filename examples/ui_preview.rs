//! Preview autter's terminal UI in the current terminal without touching any
//! configuration or contacting the server: `cargo run --example ui_preview`.
//! Walks through the onboarding screens with sample data: the brand header,
//! the mode question, a simulated browser sign-in that approves itself after
//! a few seconds, the integrations checklist, spinner outcomes, and the
//! closing line. Useful for checking a terminal from the matrix in
//! specs/roadmap/tui-onboarding-design.md.

use std::sync::mpsc;
use std::thread::sleep;
use std::time::Duration;

use autter::ui::signin::{self, SigninEnd, SigninScreen};
use autter::ui::{self, SelectItem, progress::Spinner, rail};

fn main() {
    eprintln!("{:?}", ui::caps());
    eprintln!();
    ui::Header::new(
        "Autter",
        format!("v{}", env!("CARGO_PKG_VERSION")),
        "Preview \u{00B7} nothing is saved",
        ui::header::display_cwd(),
    )
    .print();
    eprintln!();

    let caps = ui::caps();
    let items = [
        SelectItem::new(
            "Connected \u{2014} link this machine to your Autter org (recommended)",
            "Powers prompt search, team dashboards, and PR authorship views. Sign-in opens in your browser.",
        ),
        SelectItem::new(
            "Local only \u{2014} everything stays on this machine",
            "No account needed. Switch anytime with `autter onboard --connect`.",
        ),
    ];
    let question = rail::titled(&caps, "How should Autter run on this machine?", Some("1/3"));
    if ui::select(&question, &items, 0) == 0 {
        preview_sign_in();
    }

    let tools = ["Claude Code", "Cursor", "VS Code"];
    let mut items: Vec<SelectItem> = tools
        .iter()
        .map(|name| SelectItem::new(name, "Hooks record the edits this tool's agent makes."))
        .collect();
    items.push(SelectItem::new(
        "Git capture and background service",
        "Global git trace2 settings and a background service, so commits from any terminal get authorship notes.",
    ));
    let all: Vec<usize> = (0..items.len()).collect();
    let question = format!(
        "{}\n{}",
        rail::titled(&caps, "Install integrations?", Some("3/3")),
        caps.paint("2", "space toggles \u{00B7} enter confirms")
    );
    let chosen = ui::multiselect(&question, &items, &all);

    // The four outcome styles `autter install-hooks` prints per tool. Nothing
    // is installed.
    let _rail = ui::progress::on_rail();
    for (i, finish) in [
        Spinner::success as fn(&Spinner, &str),
        Spinner::pending,
        Spinner::error,
    ]
    .into_iter()
    .enumerate()
    {
        if !chosen.contains(&i) {
            continue;
        }
        let message = format!("{}: sample outcome", tools[i]);
        let spinner = Spinner::new(&message);
        sleep(Duration::from_millis(800));
        finish(&spinner, &message);
    }
    drop(_rail);

    rail::print_gap();
    rail::print_end(&format!(
        "{} Authorship is recorded automatically on every commit.",
        caps.paint("1;32", "You're all set!")
    ));
}

/// The sign-in screen with a fake approval after six seconds. Esc steps
/// away; Enter and `c` behave as in the real flow.
fn preview_sign_in() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        sleep(Duration::from_secs(6));
        let _ = tx.send(Ok::<_, String>("you@company.com \u{00B7} acme"));
    });
    let screen = SigninScreen {
        title: rail::titled(&ui::caps(), "Sign in to Autter", Some("2/3")),
        code: "WDJB-MJHT",
        url: "https://app.autter.dev/cli/authorize?code=WDJB-MJHT",
        browser_opened: false,
        expires_in: Duration::from_secs(900),
        escape_label: "continue in local mode",
    };
    match signin::wait(&screen, rx, || false) {
        SigninEnd::Approved(who) => rail::print_step(&format!("Signed in as {who}"), Some("2/3")),
        SigninEnd::Escaped => rail::print_step("Sign in \u{00B7} skipped", Some("2/3")),
        SigninEnd::Failed(error) => rail::print_warn(&format!("Sign-in didn't finish: {error}")),
    }
}
