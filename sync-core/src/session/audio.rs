//! 会话内音频链路的控制面：目标（用户期望）→ 方案（下发给 phone 的代次）→ 运行态。
//!
//! 分工：
//! - Core 通过 `watch<AudioTarget>` 只写「用户期望」，连续操作自动合并到最新值；
//! - 本模块的 [`Link`] 把期望翻译成带代次的 [`AudioPlan`]（换代次 = 丢掉上一代
//!   在途媒体），并根据 phone 的 `AudioState` 回执与音频帧进度维护运行态；
//! - runner 只负责把 `Link` 的结论落到线上（发 `SetAudio`、发布方案、推运行态）。
//!
//! WHY 单独一层：以前这套判断（等回执超时重试、确认在播后断流自救、旧代次回执
//! 忽略、关闭时释放播放资源）散在 session 主循环的 select 分支里，既读不出全貌
//! 也无法测试。现在时间与进度由调用方传入，逻辑变成纯状态机（见本文件测试）。

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kulua_proto::codec::AssembledMedia;
use kulua_proto::session::UdpSession;

use crate::audio_player::{AUDIO_BUFFER_MAX_MS, AudioCodec, AudioPlayer};

/// 音频目标（Core 写 / Session 读）：用户期望的开关与编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTarget {
    pub enabled: bool,
    /// 编码索引：0=raw 1=opus 2=aac 3=flac（与 `SetAudio.codec` 一致）。
    pub codec: u8,
}

impl AudioTarget {
    pub fn new(enabled: bool, codec: u8) -> Self {
        Self { enabled, codec }
    }
}

/// 已下发给 phone 的音频方案（runner → 音频任务）。
///
/// `revision` 单调递增：媒体帧 / `MediaConfig` / `AudioState` 都带同一代次，
/// 音频任务只接受与当前方案同代次的数据（旧代次在途媒体一律丢弃）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPlan {
    pub revision: u64,
    pub enabled: bool,
    pub codec: u8,
}

impl AudioPlan {
    /// 该代次的数据是否属于当前方案（音频任务过滤用）。
    pub fn accepts(&self, revision: u64) -> bool {
        self.enabled && revision == self.revision
    }
}

/// 音频任务上报的进度（单调递增的成功播放帧数）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub frames: u64,
}

/// 本拍需要下发的切换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issue {
    pub plan: AudioPlan,
    /// 触发原因（日志用）。
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 目标变更（开关或编码）。
    Target,
    /// 等回执超时重试。
    Retry,
    /// 已确认在播但断流，换代次自救。
    Stall,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Target => "目标变更",
            Reason::Retry => "重试",
            Reason::Stall => "断流自救",
        }
    }
}

/// 本拍结论（runner 据此执行副作用：发命令、归零指标、打日志）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Tick {
    pub issue: Option<Issue>,
    /// 本拍新判定断流（调用方归零缓冲指标）。
    pub stalled: bool,
    /// 本拍从「非在播」恢复（含断流恢复与失败后自愈）。
    pub recovered: bool,
}

// ── 运行态 ────────────────────────────────────────────────────────

/// 未启用（用户关闭，或会话初始即为关闭）。
pub const AUDIO_STATE_OFF: u8 = 0;
/// 切换中：命令已下发，等 phone 的 AudioState。
pub const AUDIO_STATE_STARTING: u8 = 1;
/// 已在采集/播放（phone 回执就绪，或帧正在流动）。
pub const AUDIO_STATE_ON: u8 = 2;
/// 关闭中：命令已下发，等 phone 停止回执。
pub const AUDIO_STATE_STOPPING: u8 = 3;
/// 失败：phone 明确报错、切换超时重试耗尽、或运行中断流。
pub const AUDIO_STATE_FAILED: u8 = 4;

/// 运行态 → IPC 字符串（UI 展示用）。
pub fn state_str(state: u8) -> &'static str {
    match state {
        AUDIO_STATE_OFF => "off",
        AUDIO_STATE_STARTING => "starting",
        AUDIO_STATE_ON => "on",
        AUDIO_STATE_STOPPING => "stopping",
        AUDIO_STATE_FAILED => "failed",
        _ => "unknown",
    }
}

/// 音频运行态快照（推 UI）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioRuntime {
    /// 当前代次（= 最后一次下发的 `SetAudio.revision`）。
    pub revision: u64,
    pub state: u8,
    /// phone 回执的实际 codec id（4B ASCII；0=未启用）。
    pub codec_id: u32,
    /// `state == FAILED` 时的原因（UI 直接展示）。
    pub error: String,
}

/// 跨任务共享的运行态（runner 写、Core tick 读）。
pub type SharedRuntime = Arc<Mutex<AudioRuntime>>;

/// 建一个默认运行态（会话启动时用）。
pub fn shared_runtime() -> SharedRuntime {
    Arc::new(Mutex::new(AudioRuntime::default()))
}

/// 读取运行态快照（Core 推 UI 用）。
///
/// 锁中毒只说明某次写入 panic 了，运行态是展示用的状态值，被一次 panic 永久
/// 毒死反而更糟——直接取回内部数据继续用。
pub fn snapshot(runtime: &SharedRuntime) -> AudioRuntime {
    match runtime.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// 发布运行态（只在变化时写入；runner 是唯一写入方）。
pub fn publish(runtime: &SharedRuntime, snapshot: AudioRuntime) {
    let mut guard = match runtime.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *guard != snapshot {
        *guard = snapshot;
    }
}

// ── 控制面状态机 ──────────────────────────────────────────────────

/// 控制面推进节拍（runner 主循环用）。
pub const TICK: Duration = Duration::from_millis(250);
/// 下发后等 `AudioState` 回执的超时。
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// 单轮切换（等回执 + 断流自救）的重试上限。
const MAX_RETRIES: u32 = 3;
/// 确认在播后多久没有新帧判为断流。
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// 断流后自动换代次的最小间隔。
const STALL_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// 音频链路控制面。
///
/// 所有判断都基于调用方传入的 `now` 与 [`Progress`]，不碰时钟/网络 → 可单测。
pub struct Link {
    /// 用户期望（`target` 与 `applied` 不同 = 需要下发）。
    applied: AudioTarget,
    /// 已下发的方案（音频任务按它过滤媒体）。
    plan: AudioPlan,
    /// 正在等回执的切换：已重试次数 + 下次重试时刻。
    pending: Option<(u32, Instant)>,
    /// 断流判定态：已自救次数 + 上次自救时刻。
    stalled: Option<(u32, Instant)>,
    /// 进度基线。
    last_frames: u64,
    last_progress: Instant,
    runtime: AudioRuntime,
}

impl Link {
    /// 建链路：`target` 为会话初始音频目标（HELLO 已带出），代次从 0 起。
    pub fn new(target: AudioTarget, now: Instant) -> Self {
        Self {
            applied: target,
            plan: AudioPlan {
                revision: 0,
                enabled: target.enabled,
                codec: target.codec,
            },
            pending: None,
            stalled: None,
            last_frames: 0,
            last_progress: now,
            runtime: AudioRuntime {
                revision: 0,
                state: if target.enabled {
                    AUDIO_STATE_STARTING
                } else {
                    AUDIO_STATE_OFF
                },
                codec_id: 0,
                error: String::new(),
            },
        }
    }

    pub fn plan(&self) -> AudioPlan {
        self.plan
    }

    pub fn runtime(&self) -> AudioRuntime {
        self.runtime.clone()
    }

    /// 推进一拍：目标变更 / 等回执超时 / 在播断流三个触发源汇总成一次结论。
    ///
    /// 重试同样换代次：可靠层按代次天然幂等（phone 只执行更大代次），
    /// 而重试必然生效，无需在 phone 侧做去重。
    pub fn drive(&mut self, now: Instant, target: AudioTarget, progress: Progress) -> Tick {
        let mut tick = Tick::default();

        // 1) 进度：帧数前进 = 当前代次确实在播（含失败/断流后的自愈）
        if progress.frames != self.last_frames {
            self.last_frames = progress.frames;
            self.last_progress = now;
            self.stalled = None;
            // 只有「当前代次本该有音频」时，帧前进才算恢复：关闭/切换过程中仍有
            // 上一批在途帧被计数，否则会把 off 误判成 on，并进而误判断流。
            if self.plan.enabled && self.runtime.state != AUDIO_STATE_ON {
                self.set_state(AUDIO_STATE_ON, None);
                tick.recovered = true;
            }
        } else if self.plan.enabled
            && self.runtime.state == AUDIO_STATE_ON
            && now.duration_since(self.last_progress) >= STALL_TIMEOUT
            && self.stalled.is_none()
        {
            // 2) 确认在播却长时间没有新帧：指标归零由调用方做，这里只改状态
            self.stalled = Some((0, now));
            self.set_state(AUDIO_STATE_FAILED, Some("音频帧断流".into()));
            tick.stalled = true;
        }

        // 3) 决定是否下发
        tick.issue = self.decide(now, target);
        tick
    }

    /// 处理 phone 的业务回执；返回是否被采纳（旧代次回执会被丢弃）。
    pub fn on_state(&mut self, revision: u64, enabled: bool, codec_id: u32, error: &str) -> bool {
        if revision < self.plan.revision {
            return false; // 上一代采集器的迟到回执
        }
        if error.is_empty() {
            self.runtime.codec_id = codec_id;
            self.set_state(
                if enabled {
                    AUDIO_STATE_ON
                } else {
                    AUDIO_STATE_OFF
                },
                None,
            );
        } else {
            self.set_state(AUDIO_STATE_FAILED, Some(error.to_string()));
        }
        if revision == self.plan.revision {
            self.pending = None; // 业务结果已到（含失败）：停止重试，失败由 UI 呈现
        }
        true
    }

    /// 决策：需要下发时返回新方案（换代次），否则 None。
    fn decide(&mut self, now: Instant, target: AudioTarget) -> Option<Issue> {
        if target != self.applied {
            return Some(self.issue(now, target, Reason::Target, 0));
        }
        let (tried, retry_at) = self.pending?;
        if now < retry_at {
            return None;
        }
        if tried >= MAX_RETRIES {
            let msg = format!("设备未确认音频切换（已重试 {tried} 次）");
            self.pending = None;
            self.set_state(AUDIO_STATE_FAILED, Some(msg));
            return None;
        }
        Some(self.issue(now, self.applied, Reason::Retry, tried + 1))
    }

    /// 断流自救：确认在播但帧已断，按固定间隔换代次重发（上限 [`MAX_RETRIES`]）。
    ///
    /// 与回执超时分开触发（一个等不到回执、一个回执正常但没帧），但共用同一套
    /// 「换代次下发」逻辑。
    pub fn poll_stall_retry(&mut self, now: Instant) -> Option<Issue> {
        let (tried, since) = self.stalled?;
        if tried >= MAX_RETRIES || now.duration_since(since) < STALL_RETRY_INTERVAL {
            return None;
        }
        Some(self.issue(now, self.applied, Reason::Stall, 0))
    }

    /// 换代次下发：目标生效、进入切换中、记录等回执。
    fn issue(&mut self, now: Instant, target: AudioTarget, reason: Reason, tried: u32) -> Issue {
        self.applied = target;
        self.plan = AudioPlan {
            revision: self.plan.revision + 1,
            enabled: target.enabled,
            codec: target.codec,
        };
        self.runtime.revision = self.plan.revision;
        self.pending = Some((tried, now + ACK_TIMEOUT));
        self.last_progress = now; // 新代次重新计时，避免立刻判断流
        self.stalled = match reason {
            Reason::Stall => {
                let (kept, _) = self.stalled.unwrap_or((0, now));
                Some((kept + 1, now))
            }
            // 目标变更 / 重试：本代次还没确认在播，断流判定从零开始
            _ => None,
        };
        self.set_state(
            if target.enabled {
                AUDIO_STATE_STARTING
            } else {
                AUDIO_STATE_STOPPING
            },
            None,
        );
        Issue {
            plan: self.plan,
            reason,
        }
    }

    fn set_state(&mut self, state: u8, error: Option<String>) {
        self.runtime.state = state;
        self.runtime.error = error.unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAC: u8 = 2;
    const CODEC_ID: u32 = 0x0061_6163;

    fn link(enabled: bool) -> (Link, Instant) {
        let now = Instant::now();
        (Link::new(AudioTarget::new(enabled, AAC), now), now)
    }

    /// 目标变更 → 换代次下发，进入切换中；phone 回执后就绪并停止重试。
    #[test]
    fn target_change_issues_then_ack_settles() {
        let (mut link, t0) = link(true);
        assert_eq!(link.plan().revision, 0, "会话初始代次为 0");
        assert_eq!(link.runtime().state, AUDIO_STATE_STARTING);

        let tick = link.drive(t0, AudioTarget::new(false, AAC), Progress::default());
        let issue = tick.issue.expect("关闭目标应下发");
        assert_eq!(issue.reason, Reason::Target);
        assert_eq!(issue.plan.revision, 1);
        assert!(!issue.plan.enabled);
        assert_eq!(link.runtime().state, AUDIO_STATE_STOPPING);

        assert!(link.on_state(1, false, 0, ""));
        assert_eq!(link.runtime().state, AUDIO_STATE_OFF);
        // 已就绪：不再下发
        assert!(
            link.drive(t0, AudioTarget::new(false, AAC), Progress::default())
                .issue
                .is_none()
        );
    }

    /// 等不到回执 → 每超时一次换代次重试，上限后标失败（且不再刷命令）。
    #[test]
    fn ack_timeout_retries_with_new_revision_then_fails() {
        let (mut link, t0) = link(false);
        let target = AudioTarget::new(true, AAC);
        let mut now = t0;

        let first = link.drive(now, target, Progress::default()).issue.unwrap();
        assert_eq!(first.plan.revision, 1, "首次下发 rev=1");

        for attempt in 1..=MAX_RETRIES {
            // 未到超时：不下发
            now += ACK_TIMEOUT - Duration::from_millis(1);
            assert!(
                link.drive(now, target, Progress::default()).issue.is_none(),
                "未到超时不应重发"
            );
            // 到点：换代次重试
            now += Duration::from_millis(1);
            let issue = link
                .drive(now, target, Progress::default())
                .issue
                .expect("超时应重试");
            assert_eq!(issue.reason, Reason::Retry);
            assert_eq!(
                issue.plan.revision,
                1 + u64::from(attempt),
                "重试必须换代次"
            );
            assert_eq!(link.runtime().revision, 1 + u64::from(attempt));
        }

        // 重试上限：标记失败且不再下发
        now += ACK_TIMEOUT;
        let tick = link.drive(now, target, Progress::default());
        assert!(tick.issue.is_none(), "超过重试上限不得继续刷命令");
        assert_eq!(link.runtime().state, AUDIO_STATE_FAILED);
        assert!(link.runtime().error.contains("未确认"));
    }

    /// 旧代次的迟到回执不得覆盖新状态。
    #[test]
    fn stale_ack_is_ignored() {
        let (mut link, t0) = link(true);
        link.drive(t0, AudioTarget::new(true, 1), Progress::default()); // rev=1（opus）
        assert!(link.on_state(1, true, CODEC_ID, ""));
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);

        link.drive(t0, AudioTarget::new(true, AAC), Progress::default()); // rev=2
        assert!(link.on_state(2, true, CODEC_ID, ""));

        // rev=1 的迟到回执（例如上一代采集器才刚就绪）
        assert!(!link.on_state(1, false, 0, "采集失败"));
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);
        assert!(link.runtime().error.is_empty());

        // 同代次内的失败迁移仍生效
        assert!(link.on_state(2, false, 0, "编码器不可用"));
        assert_eq!(link.runtime().state, AUDIO_STATE_FAILED);
        assert_eq!(link.runtime().error, "编码器不可用");
    }

    /// phone 报错 → 失败可见；随后帧恢复 → 自动翻回在播。
    #[test]
    fn failure_is_visible_and_recovers_with_frames() {
        let (mut link, t0) = link(true);
        assert!(link.on_state(0, false, 0, "AudioRecord 初始化失败"));
        assert_eq!(link.runtime().state, AUDIO_STATE_FAILED);
        assert_eq!(link.runtime().error, "AudioRecord 初始化失败");

        let tick = link.drive(t0, AudioTarget::new(true, AAC), Progress { frames: 1 });
        assert!(tick.recovered, "有帧在流即视为恢复");
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);
        assert!(link.runtime().error.is_empty());
    }

    /// 确认在播后断流 → 判断流（指标由调用方归零），随后按间隔换代次自救。
    #[test]
    fn stall_flags_then_self_heals_at_fixed_interval() {
        let (mut link, t0) = link(true);
        assert!(link.on_state(0, true, CODEC_ID, ""));
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);

        // 帧数不变且超过阈值
        let tick = link.drive(
            t0 + STALL_TIMEOUT,
            AudioTarget::new(true, AAC),
            Progress { frames: 0 },
        );
        assert!(tick.stalled);
        assert_eq!(link.runtime().state, AUDIO_STATE_FAILED);
        assert_eq!(link.runtime().error, "音频帧断流");

        // 未到自救间隔：不动
        assert!(
            link.poll_stall_retry(
                t0 + STALL_TIMEOUT + STALL_RETRY_INTERVAL - Duration::from_secs(1)
            )
            .is_none()
        );
        // 到点：换代次自救（换代次后回到切换中，等待新回执）
        let issue = link
            .poll_stall_retry(t0 + STALL_TIMEOUT + STALL_RETRY_INTERVAL)
            .expect("到点应自救");
        assert_eq!(issue.reason, Reason::Stall);
        assert_eq!(issue.plan.revision, 1);
        assert_eq!(link.runtime().state, AUDIO_STATE_STARTING);

        // 自救后帧恢复 → 回到在播，且自救计数清零
        let tick = link.drive(
            t0 + STALL_TIMEOUT + STALL_RETRY_INTERVAL,
            AudioTarget::new(true, AAC),
            Progress { frames: 7 },
        );
        assert!(tick.recovered);
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);
    }

    /// 回归（真机抓到）：关闭过程中仍有在途帧被计数，不得把 off 误判成 on，
    /// 更不得因此在 3 秒后误判断流（UI 会显示"音频失败"）。
    #[test]
    fn frames_during_disable_must_not_resurrect_link() {
        let (mut link, t0) = link(true);
        assert!(link.on_state(0, true, CODEC_ID, ""));
        assert_eq!(link.runtime().state, AUDIO_STATE_ON);

        // 下发关闭 → phone 回执 off（此时播放资源已释放、链路已关）
        let issue = link
            .drive(t0, AudioTarget::new(false, AAC), Progress::default())
            .issue
            .expect("关闭应下发");
        assert!(link.on_state(issue.plan.revision, false, 0, ""));
        assert_eq!(link.runtime().state, AUDIO_STATE_OFF);

        // 上一批在途帧此刻才被计数 → 计数前进
        let tick = link.drive(t0, AudioTarget::new(false, AAC), Progress { frames: 5 });
        assert!(!tick.recovered, "关闭中帧计数前进不算恢复");
        assert_eq!(link.runtime().state, AUDIO_STATE_OFF);

        // 之后长时间没有帧也不得判成断流
        let tick = link.drive(
            t0 + STALL_TIMEOUT * 5,
            AudioTarget::new(false, AAC),
            Progress { frames: 5 },
        );
        assert!(!tick.stalled, "关闭状态不得判断流");
        assert_eq!(link.runtime().state, AUDIO_STATE_OFF);
        assert!(link.runtime().error.is_empty());
    }

    /// 关音频后不再判断流（本来就没有帧）。
    #[test]
    fn disabled_link_never_reports_stall() {
        let (mut link, t0) = link(true);
        assert!(link.on_state(0, true, CODEC_ID, ""));
        link.drive(t0, AudioTarget::new(false, AAC), Progress::default());
        let tick = link.drive(
            t0 + STALL_TIMEOUT * 10,
            AudioTarget::new(false, AAC),
            Progress { frames: 0 },
        );
        assert!(!tick.stalled);
        assert!(link.poll_stall_retry(t0 + STALL_TIMEOUT * 20).is_none());
    }

    /// 连续目标变更只体现为一次下发（watch 已合并，`decide` 只认最新值）。
    #[test]
    fn latest_target_wins() {
        let (mut link, t0) = link(true);
        let issue = link
            .drive(t0, AudioTarget::new(false, AAC), Progress::default())
            .issue
            .unwrap();
        assert!(!issue.plan.enabled, "只下发最终目标");
        assert_eq!(issue.plan.revision, 1, "一次变更只占一个代次");
    }
}

// ── 媒体播放任务 ─────────────────────────────────────────────────

/// 音频任务输入事件（全部带代次：晚到的上一代媒体必须被丢弃）。
///
/// 目标/代次的变更不走这里，而是 `watch<AudioPlan>`（见 [`Link`]）：控制面必须
/// 立即生效且不能因通道满而阻塞主循环，媒体才走有界的 mpsc。
pub enum AudioEvent {
    /// AAC AudioSpecificConfig / FLAC STREAMINFO（可靠 MediaConfig），带代次。
    Config { revision: u64, data: Vec<u8> },
    /// 一帧编码音频，带代次。
    Frame {
        revision: u64,
        frame: AssembledMedia,
    },
}

/// 下发一次音频切换：命令走可靠 control 流，方案经 watch 直达音频任务。
///
/// WHY 方案要立刻发布：音频任务据此切代次，上一代在途的帧/config 才不会被当成
/// 当前代次播放；关闭时任务收到 `enabled=false` 即释放播放资源。
pub fn send_audio(
    udp: &UdpSession,
    plan_tx: &tokio::sync::watch::Sender<AudioPlan>,
    plan: &AudioPlan,
) -> Result<(), String> {
    udp.send_ctrl(&kulua_proto::session::msgs::set_audio(
        plan.enabled,
        u32::from(plan.codec),
        plan.revision,
    ))?;
    let _ = plan_tx.send(*plan);
    Ok(())
}

/// 音频播放任务：只做「当前代次的媒体 → PCM 播放」。
///
/// - 方案（代次/开关/编码）走 `watch<AudioPlan>`：换代次立即作废旧 config 与
///   解码器，`enabled=false` 立即释放播放资源（rodio sink + 输出设备）
/// - 代次不匹配的 Config/Frame 一律丢弃：旧代次音频不得串入新播放队列
/// - AAC/FLAC 必须等到当前代次的 config 才建解码器，否则解码器会永久失败
/// - 成功播放的帧计数发布给控制面：断流判定、指标归零与重试都在控制面
pub fn spawn_audio_task(
    mut rx: tokio::sync::mpsc::Receiver<AudioEvent>,
    mut plan_rx: tokio::sync::watch::Receiver<AudioPlan>,
    serial: String,
    volume: Arc<AtomicU16>,
    latency: Arc<AtomicU64>,
    frames: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut plan = *plan_rx.borrow();
        let mut codec_config: Option<Vec<u8>> = None;
        let mut player: Option<AudioPlayer> = None;
        let mut last_vol = volume.load(Ordering::Relaxed);
        let mut log_timer = tokio::time::Instant::now();
        let mut warned_missing_config = false;

        loop {
            tokio::select! {
                biased;
                // 方案变更优先于媒体：先切代次，再处理这一代的数据
                changed = plan_rx.changed() => {
                    if changed.is_err() {
                        break; // 会话结束
                    }
                    plan = *plan_rx.borrow();
                    // 换代次必然重建解码器：旧 config 与半解码状态都作废
                    player = None;
                    codec_config = None;
                    warned_missing_config = false;
                    if plan.enabled {
                        println!("[audio] {serial} 音频链路开启 rev={} codec={}",
                            plan.revision, codec_label(plan.codec));
                    } else {
                        latency.store(0, Ordering::Relaxed);
                        println!("[audio] {serial} 音频链路关闭（已释放播放资源）");
                    }
                }
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        AudioEvent::Config { revision, data } => {
                            if !plan.accepts(revision) {
                                continue; // 旧代次 config：丢弃
                            }
                            codec_config = Some(data);
                            player = None; // 用新 config 重建解码器
                            warned_missing_config = false;
                        }
                        AudioEvent::Frame { revision, frame } => {
                            if !plan.accepts(revision) {
                                continue; // 旧代次 / 已关闭：丢弃
                            }
                            let codec = codec_of(plan.codec);
                            // 会话元数据帧（预留）忽略
                            if frame.flags & kulua_proto::codec::MEDIA_FLAG_SESSION != 0 {
                                continue;
                            }
                            // 帧内 config 标志（防御：某些实现可能仍内联 config）
                            if frame.flags & kulua_proto::codec::MEDIA_FLAG_CONFIG != 0 {
                                codec_config = Some(frame.data.clone());
                                player = None;
                                continue;
                            }
                            if player.is_none() {
                                // 需要 config 的编码器必须等当前代次的参数集，否则建出来的
                                // 解码器会一直失败（且不会因为 config 后到而重建）
                                if codec.needs_config() && codec_config.is_none() {
                                    if !warned_missing_config {
                                        warned_missing_config = true;
                                        println!(
                                            "[audio] {serial} 等待 codec config（rev={} codec={}），暂不解码",
                                            plan.revision,
                                            codec.name()
                                        );
                                    }
                                    continue;
                                }
                                let p = match AudioPlayer::new(
                                    codec,
                                    codec_config.as_deref(),
                                ) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        eprintln!(
                                            "[audio] failed to init {} player on {serial}: {e}",
                                            codec.name()
                                        );
                                        continue;
                                    }
                                };
                                p.set_volume(volume.load(Ordering::Relaxed) as f32 / 100.0);
                                player = Some(p);
                            }
                            let player = player.as_mut().unwrap();

                            // 同步音量
                            let cur = volume.load(Ordering::Relaxed);
                            if cur != last_vol {
                                player.set_volume(cur as f32 / 100.0);
                                last_vol = cur;
                            }

                            // 积压超阈值 → 丢帧清空
                            if player.buffer_ms() > AUDIO_BUFFER_MAX_MS {
                                eprintln!(
                                    "[audio] {serial} buffer {}ms exceeded limit, flushing backlog",
                                    player.buffer_ms()
                                );
                                player.clear();
                            }
                            if let Err(e) = player.feed_frame(&frame.data) {
                                eprintln!("[audio] decode error on {serial}: {e}");
                            } else {
                                latency.store(player.buffer_ms(), Ordering::Relaxed);
                                frames.fetch_add(1, Ordering::Relaxed);
                            }

                            if log_timer.elapsed() >= Duration::from_secs(5) {
                                eprintln!("[audio] {serial} buffer: {} ms", player.buffer_ms());
                                log_timer = tokio::time::Instant::now();
                            }
                        }
                    }
                }
            }
        }
    })
}

/// 编码索引 → 名称（日志用）。
fn codec_label(index: u8) -> &'static str {
    codec_of(index).name()
}

/// 编码索引（`SetAudio.codec`）→ 编码器。
fn codec_of(index: u8) -> AudioCodec {
    AudioCodec::from_handshake_byte(index).unwrap_or(AudioCodec::Raw)
}
