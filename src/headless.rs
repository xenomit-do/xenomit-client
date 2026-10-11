use ruffle_core::backend::audio::NullAudioBackend;
use ruffle_core::{FloatDuration, PlayerBuilder};
use ruffle_render::backend::ViewportDimensions;
use ruffle_render::backend::null::NullRenderer;
use ruffle_render::quality::StageQuality;

use crate::authenticator::PreparedLaunch;
use crate::executor::GameExecutor;
use crate::navigator::GameNavigator;
use std::time::{Duration, Instant};

pub async fn run_headless(
    launch: PreparedLaunch,
    script_timeout: u64,
    quality: StageQuality,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("[Mode] Running headless");

    let viewport = ViewportDimensions {
        width: launch.width,
        height: launch.height,
        scale_factor: 1.0,
    };
    let mut executor = GameExecutor::new();
    let navigator = GameNavigator::new(&executor, &launch.network, launch.smoke_pool)?;
    let bridge = launch.bridge;
    let page_url = launch.network.page_url.clone();
    let player = PlayerBuilder::new()
        .with_renderer(NullRenderer::new(viewport))
        .with_audio(NullAudioBackend::new())
        .with_navigator(navigator)
        .with_external_interface(Box::new(bridge.provider(Some(page_url.clone()))))
        .with_page_url(Some(page_url))
        .with_autoplay(true)
        .with_max_execution_duration(Duration::from_secs(script_timeout))
        .with_quality(quality)
        .with_movie(launch.movie)
        .build();
    bridge.spawn_pipe();

    let mut last_tick = Instant::now();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        executor.run();
        let now = Instant::now();
        let elapsed = now.duration_since(last_tick);
        last_tick = now;
        let delay = {
            let mut player = player.lock().expect("player lock");
            player.tick(FloatDuration::from_std(elapsed));
            bridge.poll(&mut player);
            player.time_til_next_frame()
        };
        executor.run();
        if bridge.close_requested() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(delay.clamp(Duration::from_millis(1), Duration::from_millis(16))) => {}
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}
