use clap::Parser;

mod headed;
mod headless;

mod authenticator;
mod desktop_ui;
mod diagnostics;
mod executor;
mod navigator;
mod swf_patch;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run the Flash player without a game window (authentication is always headless).
    #[arg(long)]
    headless: bool,

    /// Local SWF bytes to play instead of downloading the preloader.
    #[arg(long)]
    swf: Option<String>,

    /// Domain to use for the session, e.g. `int1` or `int1.darkorbit.com`.
    #[arg(short, long, default_value = "www")]
    domain: String,

    /// dosid cookie value for the game session.
    #[arg(long)]
    dosid: String,

    /// DarkOrbit renderer mode. DarkBot uses 2 unless 3D is enabled.
    #[arg(long, default_value = "2")]
    display2d: String,

    /// Wall-clock limit in seconds for one batch of AVM2 actions before the
    /// player aborts the script.
    #[arg(long, default_value_t = 60)]
    script_timeout: u64,

    /// Stage render quality.
    #[arg(long, value_enum, default_value = "high")]
    quality: QualityArg,

    /// Cap engine/rocket smoke pool sizes in main.swf at N, patching the SWF
    /// on the fly as it is downloaded.
    #[arg(long)]
    smoke_pool: Option<u32>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum QualityArg {
    Low,
    Medium,
    High,
    Best,
}

impl QualityArg {
    fn stage_quality(self) -> ruffle_render::quality::StageQuality {
        use ruffle_render::quality::StageQuality;
        match self {
            QualityArg::Low => StageQuality::Low,
            QualityArg::Medium => StageQuality::Medium,
            QualityArg::High => StageQuality::High,
            QualityArg::Best => StageQuality::Best,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    diagnostics::install();
    let args = Args::parse();

    let launch = authenticator::authenticate(
        &args.domain,
        &args.dosid,
        &args.display2d,
        args.swf.as_deref(),
        args.smoke_pool,
    )
    .await?;

    if args.headless {
        headless::run_headless(launch, args.script_timeout, args.quality.stage_quality()).await?;
    } else {
        headed::run_headed(launch, args.script_timeout, args.quality.stage_quality()).await?;
    }

    Ok(())
}
