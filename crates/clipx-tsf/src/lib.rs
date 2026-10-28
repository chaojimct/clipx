// clipx-caret-beacon — TSF TIP「光标信标」
//
// 原理（详见 docs/tsf-caret-research.md）：
// - 注册成键盘类 TIP，被 TSF 注入到所有宿主进程（WorkBuddy / 微信 / Cursor …）。
// - 在宿主进程内只做一件事：读出真实插入点矩形
//   （ITfContextView::GetTextExt），写进共享内存 `Local\clipx-tsf-caret`。
// - clipx 侧读共享内存即可拿到**像素级精确**的光标位置，
//   从而绕开 UIA/IMM 在 Electron 应用上「拿不到自绘 caret」的死结。
//
// 三条铁律（任何改动都必须遵守）：
// 1. 绝不吞键：ITfKeyEventSink 一律返回 FALSE，键全部透传给应用。
// 2. 激活期间把「键盘开关」compartment 置 0（IME 直通 = 英文直通），
//    Deactivate 必须恢复为 1，否则下一个 IME 会卡在英文态打不出中文。
// 3. Deactivate **保留** sink：信标要常驻宿主进程持续应答 clipx 的测量请求，
//    用户真正的输入法（微信输入法等）照常活动，两者互不干扰。
//
// 共享内存布局（Local\clipx-tsf-caret，64 槽 × 64 字节）：
//   槽 0 = 控制块
//     [0] magic 0x43424332 ("BC2")
//     [1] version (=1)
//     [2] request_seq   clipx 递增，表示「请测一次」
//     [3] request_tick_lo / [4] request_tick_hi
//     [5] ack_seq       信标已应答的 request_seq
//     [6] flags
//   槽 1..=63 = 测量结果环形缓冲
//     [0] magic 0x43424331 ("BC1")
//     [1] pid           宿主进程 id
//     [2] tick_lo / [3] tick_hi
//     [4..7] rect l,t,r,b（宿主坐标系）
//     [8] dpi_aware
//     [9] seq
//     [10] source（0=激活期测量，1=常驻应答）

#![allow(non_snake_case)]

#[cfg(windows)]
mod win {
    use std::cell::{Cell, RefCell};
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows::core::{
        implement, BOOL, Error, GUID, HRESULT, IUnknown, IUnknownImpl, Interface, Ref, Result,
    };
    use windows::Win32::Foundation::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Memory::*;
        use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::TextServices::*;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetClientRect, GetForegroundWindow, GetWindowRect, HC_ACTION,
    };

    pub const CLSID_BEACON: GUID =
        GUID::from_u128(0xc4be0c0a_9326_4c1a_beac_0cbeac0dec01);
    pub const PROFILE_BEACON: GUID =
        GUID::from_u128(0xc4be0c0a_9326_4c1a_beac_0cbeac0dec02);

    const SHM_NAME: windows::core::PCWSTR = windows::core::w!(r"Local\clipx-tsf-caret");
    const SLOT_COUNT: usize = 64;
    const SLOT_SIZE: usize = 64;
    const MAGIC_CTRL: u32 = 0x4342_4332; // "BC2"
    const MAGIC_SLOT: u32 = 0x4342_4331; // "BC1"
    const PROTOCOL_VERSION: u32 = 1;

    fn log(msg: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join("clipx_caret_beacon.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "[{ts}ms] pid={} {msg}", std::process::id());
        }
    }

    // ------------------------------------------------------------ factory

    #[implement(IClassFactory)]
    struct Factory;

    impl IClassFactory_Impl for Factory_Impl {
        fn CreateInstance(
            &self,
            punkouter: Ref<IUnknown>,
            riid: *const GUID,
            ppvobject: *mut *mut core::ffi::c_void,
        ) -> Result<()> {
            if punkouter.ok().is_ok() {
                return Err(Error::from_hresult(CLASS_E_NOAGGREGATION));
            }
            unsafe {
                let obj: ITfTextInputProcessor = Beacon::default().into();
                let unk: IUnknown = obj.cast()?;
                unk.query(riid, ppvobject).ok()?;
            }
            Ok(())
        }

        fn LockServer(&self, _flock: BOOL) -> Result<()> {
            Ok(())
        }
    }

    // ------------------------------------------------------------ beacon

    #[implement(
        ITfTextInputProcessor,
        ITfThreadMgrEventSink,
        ITfTextEditSink,
        ITfKeyEventSink
    )]
    #[derive(Default)]
    struct Beacon {
        mgr: RefCell<Option<ITfThreadMgr>>,
        client_id: Cell<u32>,
        tm_sink_cookie: Cell<u32>,
        edit_sink_cookie: Cell<u32>,
        sink_ctx: RefCell<Option<ITfContext>>,
        push_sink_cookie: Cell<u32>,
        push_ctx: RefCell<Option<ITfContext>>,
    }

    impl ITfTextInputProcessor_Impl for Beacon_Impl {
        fn Activate(&self, ptim: Ref<ITfThreadMgr>, tid: u32) -> Result<()> {
            let tim = ptim.ok()?.clone();
            log(&format!(
                "Activate tid={tid} thread={}",
                unsafe { GetCurrentThreadId() }
            ));
            unsafe {
                // 键事件 sink：只观察、绝不吞键。
                let km: ITfKeystrokeMgr = tim.cast()?;
                let keysink: ITfKeyEventSink = self.to_object().to_interface();
                match km.AdviseKeyEventSink(tid, &keysink, true) {
                    Ok(()) => log("AdviseKeyEventSink ok (passthrough)"),
                    Err(e) => log(&format!(
                        "AdviseKeyEventSink failed hr=0x{:08X}",
                        e.code().0
                    )),
                }

                // 键盘置「关闭」= IME 直通（英文直通），键原样上屏。
                set_keyboard_open(&tim, tid, 0, "activate");

                attach_sink(self, &tim, tid);
            }
            Ok(())
        }

        fn Deactivate(&self) -> Result<()> {
            // 铁律 2：必须把键盘开关恢复成「打开」，
            // 否则用户切回微信输入法后会卡在英文态（打不出中文）。
            let tid = self.client_id.get();
            if let Some(tim) = self.mgr.borrow().as_ref() {
                unsafe {
                    set_keyboard_open(tim, tid, 1, "deactivate");
                }
            }
            // 铁律 3：sink 故意保留 —— 信标要常驻宿主进程，
            // 继续应答 clipx 的测量请求；用户的输入法照常活动。
            log("Deactivate (sinks kept, keyboard restored open)");
            Ok(())
        }
    }

    /// 挂上只读观察用的 sink（ThreadMgr 事件 + 焦点上下文 TextEditSink）。
    ///
    /// 两种模式共用：
    /// - TIP 模式：`ITfTextInputProcessor::Activate` 由 TSF 调用。
    /// - 注入模式：`ClipxBeaconStart` 由 clipx 注入后调用（不碰 profile，
    ///   **完全不影响用户的输入法**）。
    ///
    /// 关键：**不**在这里动 keyboard openclose —— 那是 TIP 模式专属
    /// （TIP 被激活时需要置 0 走英文直通；注入模式压根不该碰输入法状态）。
    unsafe fn attach_sink(beacon: &Beacon_Impl, tim: &ITfThreadMgr, tid: u32) {
        unsafe {
            let source: ITfSource = tim.cast().expect("ITfSource");
            let sink: ITfThreadMgrEventSink = beacon.to_object().to_interface();
            match source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink) {
                Ok(cookie) => beacon.tm_sink_cookie.set(cookie),
                Err(e) => log(&format!("AdviseSink failed hr=0x{:08X}", e.code().0)),
            }

            *beacon.mgr.borrow_mut() = Some(tim.clone());
            beacon.client_id.set(tid);
            {
                let mut g = global().lock().unwrap();
                g.mgr = Some(tim.clone());
                g.client_id = tid;
            }
        }
        measure_caret(tim, tid);
        ensure_worker();
    }

    unsafe fn set_keyboard_open(tim: &ITfThreadMgr, tid: u32, open: i32, why: &str) {
        let Ok(compartment_mgr) = tim.cast::<ITfCompartmentMgr>() else {
            return;
        };
        let Ok(compartment) = compartment_mgr.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)
        else {
            return;
        };
        let v: i32 = open;
        match compartment.SetValue(tid, &v.into()) {
            Ok(()) => log(&format!("keyboard openclose = {open} ({why})")),
            Err(e) => log(&format!(
                "set openclose failed hr=0x{:08X}",
                e.code().0
            )),
        }
    }

    impl ITfThreadMgrEventSink_Impl for Beacon_Impl {
        fn OnInitDocumentMgr(&self, _pdim: Ref<ITfDocumentMgr>) -> Result<()> {
            Ok(())
        }

        fn OnUninitDocumentMgr(&self, _pdim: Ref<ITfDocumentMgr>) -> Result<()> {
            Ok(())
        }

        fn OnSetFocus(&self, pdimnew: Ref<ITfDocumentMgr>, _pdimold: Ref<ITfDocumentMgr>) -> Result<()> {
            unsafe {
                if let Some(old) = self.sink_ctx.borrow_mut().take() {
                    let cookie = self.edit_sink_cookie.replace(0);
                    if cookie != 0 {
                        if let Ok(src) = old.cast::<ITfSource>() {
                            let _ = src.UnadviseSink(cookie);
                        }
                    }
                }
                let Ok(dm) = pdimnew.ok() else { return Ok(()) };
                let Ok(ctx) = dm.GetBase() else { return Ok(()) };
                let Ok(source) = ctx.cast::<ITfSource>() else { return Ok(()) };
                let sink: ITfTextEditSink = self.to_object().to_interface();
                let Ok(cookie) = source.AdviseSink(&ITfTextEditSink::IID, &sink) else {
                    return Ok(());
                };
                self.edit_sink_cookie.set(cookie);
                *self.sink_ctx.borrow_mut() = Some(ctx);
                log("OnSetFocus: TextEditSink advised");
            }
            if let Some(tim) = self.mgr.borrow().as_ref() {
                let tid = self.client_id.get();
                measure_caret(tim, tid);
            }
            Ok(())
        }

        fn OnPushContext(&self, pic: Ref<ITfContext>) -> Result<()> {
            unsafe {
                if let Ok(ctx) = pic.ok().map(|c| c.clone()) {
                    if let Ok(source) = ctx.cast::<ITfSource>() {
                        let sink: ITfTextEditSink = self.to_object().to_interface();
                        if let Ok(cookie) = source.AdviseSink(&ITfTextEditSink::IID, &sink) {
                            self.push_sink_cookie.set(cookie);
                            *self.push_ctx.borrow_mut() = Some(ctx);
                            log("OnPushContext: TextEditSink advised");
                        }
                    }
                }
            }
            Ok(())
        }

        fn OnPopContext(&self, _pic: Ref<ITfContext>) -> Result<()> {
            if let Some(old) = self.push_ctx.borrow_mut().take() {
                let cookie = self.push_sink_cookie.replace(0);
                if cookie != 0 {
                    unsafe {
                        if let Ok(src) = old.cast::<ITfSource>() {
                            let _ = src.UnadviseSink(cookie);
                        }
                    }
                }
            }
            Ok(())
        }
    }

    impl ITfTextEditSink_Impl for Beacon_Impl {
        fn OnEndEdit(
            &self,
            pic: Ref<ITfContext>,
            _ecold: u32,
            _ecnew: Ref<ITfEditRecord>,
        ) -> Result<()> {
            // 编辑结束说明光标可能动了。这里不直接读 GetTextExt
            // （残留态下 edit cookie 已失效，实测 hr=0x80040206），
            // 改为主动发起一次只读编辑会话测量。
            if let Ok(ctx) = pic.ok() {
                let _ = measure_context(&ctx, self.client_id.get());
            }
            Ok(())
        }
    }

    impl ITfKeyEventSink_Impl for Beacon_Impl {
        fn OnSetFocus(&self, _fforeground: BOOL) -> Result<()> {
            Ok(())
        }
        fn OnTestKeyDown(
            &self,
            _pic: Ref<ITfContext>,
            _wparam: WPARAM,
            _lparam: LPARAM,
        ) -> Result<BOOL> {
            Ok(FALSE) // 绝不吞键
        }
        fn OnTestKeyUp(&self, _pic: Ref<ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
            Ok(FALSE)
        }
        fn OnKeyDown(&self, _pic: Ref<ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
            Ok(FALSE)
        }
        fn OnKeyUp(&self, _pic: Ref<ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
            Ok(FALSE)
        }
        fn OnPreservedKey(&self, _pic: Ref<ITfContext>, _rguid: *const GUID) -> Result<BOOL> {
            Ok(FALSE)
        }
    }

    // ------------------------------------------------------------ 测量

    // 本次只读测量的矩形（同线程同步会话内有效）。
    // `DoEditSession` 写入，`measure_context` 读走；无焦点枚举路径靠它做发布前校验，
    // 避免垃圾矩形污染结果槽（会盖掉别的有效结果）。
    thread_local! {
        static LAST_RECT: Cell<Option<(i32, i32, i32, i32)>> = const { Cell::new(None) };
        /// `false` 时 `DoEditSession` 只记账不发布（无焦点枚举先验后发）。
        static PUBLISH_ON: Cell<bool> = const { Cell::new(true) };
    }

    #[implement(ITfEditSession)]
    struct SelectionRect {
        context: ITfContext,
        rect: Cell<Option<RECT>>,
    }

    impl ITfEditSession_Impl for SelectionRect_Impl {
        fn DoEditSession(&self, ec: u32) -> Result<()> {
            let ctx = &self.context;
            unsafe {
                let mut sel = [TF_SELECTION::default(); 1];
                let mut fetched = 0u32;
                ctx.GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)?;
                if fetched == 0 {
                    return Ok(());
                }
                let range = (*sel[0].range)
                    .clone()
                    .ok_or_else(|| Error::from_hresult(E_FAIL))?;
                let view = ctx.GetActiveView()?;
                let mut rect = RECT::default();
                let mut clipped = BOOL::default();
                view.GetTextExt(ec, &range, &mut rect, &mut clipped)?;
                log(&format!(
                    "MEASURED rect=({},{})-({},{}) clipped={}",
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    clipped.as_bool()
                ));
                LAST_RECT.with(|c| {
                    c.set(Some((rect.left, rect.top, rect.right, rect.bottom)))
                });
                if PUBLISH_ON.with(|c| c.get()) {
                    publish_slot(rect, 1);
                }
                self.rect.set(Some(rect));
            }
            Ok(())
        }
    }

    fn measure_context(ctx: &ITfContext, tid: u32) -> Option<(i32, i32, i32, i32)> {
        LAST_RECT.with(|c| c.set(None));
        unsafe {
            let session: ITfEditSession = SelectionRect {
                context: ctx.clone(),
                rect: Cell::new(None),
            }
            .into();
            if let Err(e) = ctx.RequestEditSession(tid, &session, TF_ES_SYNC | TF_ES_READ) {
                log(&format!(
                    "measure: RequestEditSession failed hr=0x{:08X}",
                    e.code().0
                ));
            }
        }
        LAST_RECT.with(|c| c.get())
    }

    fn measure_caret(tim: &ITfThreadMgr, tid: u32) {
        unsafe {
            match tim.GetFocus() {
                Ok(dm) => match dm.GetBase() {
                    Ok(ctx) => {
                        if measure_context(&ctx, tid).is_some() {
                            return;
                        }
                        log("measure: focused doc gave nothing; trying unfocused docs");
                    }
                    Err(_) => log("measure: no base context; trying unfocused docs"),
                },
                Err(_) => log("measure: thread mgr has no focus; trying unfocused docs"),
            }
            measure_unfocused_docs(tim, tid);
        }
    }

    /// 焦点缺失时的恢复链：枚举本线程全部文档，逐个只读测量，
    /// 只发布落在前台窗内（屏幕/客户区坐标都认）的矩形，命中后顺手恢复焦点。
    ///
    /// 背景：Qt 宿主跨显示器后不再重建 TSF 焦点（副屏实测：焦点丢了十几分钟不回来，
    /// Alt+Tab 切出去再回来也救不回），但文档本身可能还在。
    /// 只读测量不碰输入法；发布前先验后发，避免垃圾矩形污染结果槽；
    /// `SetFocus` 把 TSF 焦点对齐到本就拥有键盘焦点的输入框，后续探测走正常路。
    /// 上限 4 个文档，避免槽 spam。
    fn measure_unfocused_docs(tim: &ITfThreadMgr, tid: u32) {
        unsafe {
            let Ok(en) = tim.EnumDocumentMgrs() else {
                log("measure: enum docs failed");
                return;
            };
            // 枚举路径先验后发：DoEditSession 只记账不发布。
            PUBLISH_ON.with(|c| c.set(false));
            let mut tried = 0u32;
            let mut restored = false;
            for _ in 0..4 {
                let mut arr: [Option<ITfDocumentMgr>; 1] = [None];
                let mut fetched = 0u32;
                if en.Next(&mut arr, &mut fetched).is_err() || fetched == 0 {
                    break;
                }
                let Some(dm) = arr[0].take() else { break };
                tried += 1;
                let Ok(ctx) = dm.GetBase() else {
                    log("measure: enum doc has no base context");
                    continue;
                };
                match measure_context(&ctx, tid) {
                    Some(r) if rect_plausible_in_fg(r) => {
                        // 恢复焦点（Qt 在跨屏后丢了这次 SetFocus），后续探测走正常路；
                        // 再发布本次矩形，本次探测直接成功。
                        if tim.SetFocus(&dm).is_ok() {
                            log("measure: focus restored on unfocused doc");
                            restored = true;
                        } else {
                            log("measure: SetFocus failed");
                        }
                        PUBLISH_ON.with(|c| c.set(true));
                        publish_slot(
                            RECT {
                                left: r.0,
                                top: r.1,
                                right: r.2,
                                bottom: r.3,
                            },
                            1,
                        );
                        PUBLISH_ON.with(|c| c.set(false));
                        log(&format!(
                            "measure: unfocused publish rect=({},{})-({},{})",
                            r.0, r.1, r.2, r.3
                        ));
                        break;
                    }
                    Some(r) => {
                        log(&format!(
                            "measure: enum doc rect=({},{})-({},{}) implausible, skip",
                            r.0, r.1, r.2, r.3
                        ));
                    }
                    None => {
                        log("measure: enum doc gave nothing");
                    }
                }
            }
            PUBLISH_ON.with(|c| c.set(true));
            log(&format!(
                "measure: enum docs done, tried {tried} restored={restored}"
            ));
        }
    }

    /// 候选矩形是否 plausibly 是前台窗口的光标：屏幕坐标或客户区坐标，
    /// 只要中心落在前台窗内就认（与 clipx 侧 `to_screen_point` 同判据，不含兜底）。
    fn rect_plausible_in_fg(r: (i32, i32, i32, i32)) -> bool {
        let (cx, cy) = ((r.0 + r.2) / 2, (r.1 + r.3) / 2);
        let Some((wl, wt, wr, wb)) = fg_window_rect() else {
            return false;
        };
        if wl <= cx && cx <= wr && wt <= cy && cy <= wb {
            return true;
        }
        if let Some((ox, oy)) = client_origin() {
            let (sx, sy) = (cx + ox, cy + oy);
            if wl <= sx && sx <= wr && wt <= sy && sy <= wb {
                return true;
            }
        }
        false
    }

    fn fg_window_rect() -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return None;
            }
            let mut rc = RECT::default();
            GetWindowRect(fg, &mut rc).ok()?;
            Some((rc.left, rc.top, rc.right, rc.bottom))
        }
    }

    fn client_origin() -> Option<(i32, i32)> {
        use windows::Win32::Foundation::POINT;
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                return None;
            }
            let mut rc = RECT::default();
            GetClientRect(fg, &mut rc).ok()?;
            let mut pt = POINT { x: rc.left, y: rc.top };
            let _ = ClientToScreen(fg, &mut pt);
            Some((pt.x, pt.y))
        }
    }

    // ------------------------------------------------------------ 路线乙：注入测量
    //
    // clipx 用 `SetWindowsHookEx` 把本 DLL 挂到前台窗口所属线程，
    // 钩子回调在**宿主 UI 线程内**跑一次只读测量（ITfContextView::GetTextExt）。
    //
    // 与路线甲（注册成 TIP profile）的本质区别 —— 全程**不碰输入法**：
    // - 不调用任何 profile API ⇒ 不销毁/重建用户的 TIP 实例，
    //   微信输入法的中/英态（存在它自己实例内存里的 keyboard openclose）原样保留；
    // - 不动 keyboard openclose compartment；
    // - 不注册 HKLM ⇒ Win+Space 输入法列表里不会多出一项。
    //
    // 只在 clipx 写下 pending 请求（request_seq != ack_seq）时才动手测一次，
    // 其余时候纯透传，开销可忽略；测完 clipx 立刻 UnhookWindowsHookEx（用完即摘）。

    /// 在当前线程（= 宿主 UI 线程）上做一次测量并写共享内存。
    fn measure_current_thread() {
        unsafe {
            let tim: ITfThreadMgr = match CoCreateInstance::<Option<&IUnknown>, ITfThreadMgr>(
                &CLSID_TF_ThreadMgr,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                Ok(t) => t,
                Err(e) => {
                    log(&format!("inject: CoCreateInstance(ITfThreadMgr) failed hr=0x{:08X}", e.code().0));
                    return;
                }
            };
            // 取本线程的 client id。宿主线程若已 Activate 过 TSF，
            // 通常返回同一个 id；失败则放弃本次测量（对宿主零副作用）。
            let tid = match tim.Activate() {
                Ok(t) => t,
                Err(e) => {
                    log(&format!(
                        "inject: thread mgr Activate failed hr=0x{:08X}",
                        e.code().0
                    ));
                    return;
                }
            };
            log(&format!("inject: measuring on thread={}", GetCurrentThreadId()));
            measure_caret(&tim, tid);
        }
    }

    /// `WH_GETMESSAGE` 钩子。被 SetWindowsHookEx 注入到宿主进程后，
    /// 宿主线程每取一条消息就进这里一次。
    #[no_mangle]
    pub unsafe extern "system" fn ClipxCaretHookProc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // 无条件入口记账：区分「回调根本没被调用」与「调用了但没看到请求」。
        static ENTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let enter = ENTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if enter < 5 {
            log(&format!(
                "hook entered #{enter} code={code} tid={}",
                GetCurrentThreadId()
            ));
        }
        if code == HC_ACTION as i32 {
            // 诊断：前若干次进入无条件记账，用来区分「回调没进」与
            // 「进了但没看到新请求」——这两种情况上层表现都是 TIMEOUT。
            static HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let hit = HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match shm() {
                Some(base) => {
                    let req = read_u32(base, 2);
                    let ack = read_u32(base, 5);
                    if req != 0 && req != ack {
                        log(&format!("inject hook: request_seq={req} -> measuring"));
                        measure_current_thread();
                        core::ptr::write_volatile((base as *mut u32).add(5), req);
                    } else if hit < 3 {
                        log(&format!(
                            "inject hook: idle hit={hit} tid={} req={req} ack={ack} magic={:#X}",
                            GetCurrentThreadId(),
                            read_u32(base, 0)
                        ));
                    }
                }
                None => {
                    if hit < 3 {
                        log(&format!(
                            "inject hook: shm() None hit={hit} tid={}",
                            GetCurrentThreadId()
                        ));
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    // ------------------------------------------------------------ 自装钩子（32 位宿主）
    //
    // 为什么需要这条路：
    // `SetWindowsHookEx` 要求「装钩子的进程」与「目标线程」同为 32 位或同为 64 位。
    // clipx.exe 是 x64，给 32 位宿主（企业微信 WXWork、WPS、腾讯会议…）装钩子会
    // 直接失败 —— 之前这些进程只能整体跳过信标，落点退回启发式。
    //
    // 解法：x64 clipx 用 `LoadLibrary` + 远线程把**本 DLL 的 x86 版本**注入宿主，
    // 再由 DLL 在宿主进程内自己调 `SetWindowsHookEx`（此时同位数，天然合法）。
    // 入口就是下面这个导出，由 clipx 远程调用一次；DLL 自己常驻、自己管钩子。

    /// 自装钩子的句柄集合：**不能**被 Drop（DLL 卸载前必须一直挂着）。
    /// 存 `isize` 而非 `HHOOK`：`*mut c_void` 不满足 `Send`，无法放进 static。
    static SELF_HOOKS: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());

    /// 钩子线程就绪标志：-1 = 未完成，>=0 = 已装好的钩子数。
    static SELFHOOK_READY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    /// 本模块的 HINSTANCE —— 从任意函数地址反查（不依赖 DllMain 保存）。
    fn self_hinstance() -> Option<windows::Win32::Foundation::HINSTANCE> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::System::LibraryLoader::{
            GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        unsafe {
            let mut hmod = HMODULE::default();
            let addr = PCWSTR(ClipxBeaconStart as *const u16);
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                addr,
                &mut hmod,
            )
            .ok()?;
            Some(windows::Win32::Foundation::HINSTANCE(hmod.0))
        }
    }

    /// 给本进程**所有线程**挂 `WH_GETMESSAGE`。
    ///
    /// 逐线程装（而不是 `dwThreadId=0` 的全局钩子）：全局钩子会波及同桌面所有进程，
    /// 副作用大且需要额外权限；线程级钩子只影响本进程，和 clipx 现有行为一致。
    ///
    /// **`hMod` 传本 DLL 的 HINSTANCE**：微软文档说「dwThreadId 指向本进程线程、
    /// 且钩子过程也在本进程代码里时 hMod 应为 NULL」，但实测（2026-10-23）
    /// 传 NULL 时 `SetWindowsHookEx` 返回成功、**回调永远不触发**；传 DLL 模块句柄
    /// 才真正生效（WeMail 实测 `inject hook: request_seq=238 -> measuring`）。
    /// 少数线程会返回 `ERROR_INVALID_PARAMETER`，属正常（多为已退出的陈旧线程），
    /// 只要目标 UI 线程在成功列表里即可。
    fn hook_own_threads() -> usize {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
            THREADENTRY32,
        };
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowsHookExW, HOOKPROC, WH_GETMESSAGE,
        };

        let hook_proc: HOOKPROC = Some(ClipxCaretHookProc);
        let Some(hinst) = self_hinstance() else {
            log("selfhook: self_hinstance failed");
            return 0;
        };

        unsafe {
            // **关键前置**：`CreateRemoteThread` 创建的线程默认**没有消息队列**
            // （没有 `tagTHREADINFO` 的队列结构）。在这种线程上 `SetWindowsHookEx`
            // 会返回成功句柄，但目标线程的钩子链永远不生效 —— 回调一次都不触发。
            // `PeekMessage(PM_NOREMOVE)` 的作用就是给调用线程懒创建消息队列。
            // 实测（2026-10-23）：不调用它 → `installed=N` 但零回调；
            // 调用后回调正常进入。
            let mut qmsg =
                windows::Win32::UI::WindowsAndMessaging::MSG::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut qmsg,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
            );
            log(&format!(
                "selfhook: queue ready tid={}",
                windows::Win32::System::Threading::GetCurrentThreadId()
            ));

            let me = GetCurrentProcessId();
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                log("selfhook: CreateToolhelp32Snapshot failed");
                return 0;
            };
            let mut te = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut installed: Vec<u32> = Vec::new();
            let mut failed: Vec<(u32, String)> = Vec::new();
            let mut guard = match SELF_HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if Thread32First(snap, &mut te).is_ok() {
                loop {
                    if te.th32OwnerProcessID == me {
                        match SetWindowsHookExW(
                            WH_GETMESSAGE,
                            hook_proc,
                            Some(hinst),
                            te.th32ThreadID,
                        ) {
                            Ok(h) => {
                                guard.push(h.0 as isize);
                                installed.push(te.th32ThreadID);
                            }
                            Err(e) => failed.push((te.th32ThreadID, format!("{e}"))),
                        }
                    }
                    if Thread32Next(snap, &mut te).is_err() {
                        break;
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(snap);
            // 全量 tid 记账：上层只报「TIMEOUT」时，这里能看出目标 UI 线程到底装上没有。
            log(&format!(
                "selfhook: installed={} {:?} failed={:?} pid={me} hinst={:?}",
                installed.len(),
                installed,
                failed
                    .iter()
                    .map(|(t, e)| format!("{t}({e})"))
                    .collect::<Vec<_>>(),
                hinst.0
            ));
            installed.len()
        }
    }

    /// 远程注入入口：x64 clipx 用 `CreateRemoteThread` 调本导出一次。
    ///
    /// 返回值 = 已挂上的钩子数（0 表示失败，clipx 会回落老链路）。
    /// 幂等：重复调用不会重复装。
    ///
    /// **为什么不在调用线程里直接装钩子**：`CreateRemoteThread` 创建的线程没有
    /// 窗口、也没有 desktop 关联（`tagTHREADINFO` 里没有 `pDeskInfo`），在这种
    /// 线程上 `SetWindowsHookEx` 会**返回成功句柄但钩子链永远不生效** —— 实测
    /// 目标线程一条回调都收不到（2026-10-23，`installed=N` 但零 `hook entered`）。
    /// 同一个函数改从宿主主线程调用（消息循环里、有窗口）则立刻正常。
    ///
    /// 所以这里起一条**自己的钩子线程**：先建一个 message-only 窗口让线程获得
    /// desktop 关联，再装钩子，然后跑消息循环保持存活。
    #[no_mangle]
    pub extern "system" fn ClipxBeaconStart() -> i32 {
        if let Ok(g) = SELF_HOOKS.lock() {
            if !g.is_empty() {
                return g.len() as i32;
            }
        }
        // 用一次性开关保证并发调用只起一条钩子线程。
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("clipx-beacon-selfhook".into())
                .spawn(|| {
                    hook_thread_main();
                });
        });
        // 等钩子线程就绪（装钩子很快，最多 800ms）。
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(800);
        loop {
            let v = SELFHOOK_READY.load(std::sync::atomic::Ordering::Acquire);
            if v >= 0 {
                return v;
            }
            if std::time::Instant::now() > deadline {
                log("selfhook: 钩子线程 800ms 内未就绪");
                return 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// 钩子线程主体：建 message-only 窗口 → 给本进程所有线程装钩子 → 消息循环。
    ///
    /// 窗口不是为了显示，只是让本线程获得 desktop 关联 —— 这是
    /// `SetWindowsHookEx` 生效的前提（见 `ClipxBeaconStart` 注释）。
    fn hook_thread_main() -> usize {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
            TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
        };

        unsafe extern "system" fn msg_wndproc(
            hwnd: windows::Win32::Foundation::HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        let hinst = self_hinstance();
        unsafe {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(msg_wndproc),
                hInstance: hinst.unwrap_or_default(),
                lpszClassName: windows::core::w!("clipxBeaconHookWnd"),
                ..Default::default()
            };
            let atom = RegisterClassW(&wc);
            // 重复注册（ERROR_CLASS_ALREADY_EXISTS）不算失败。
            if atom == 0 {
                log("selfhook: RegisterClassW 失败（可能已注册）");
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("clipxBeaconHookWnd"),
                windows::core::w!("clipx-beacon"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                hinst,
                None,
            );
            match hwnd {
                Ok(h) => log(&format!("selfhook: message-only hwnd={:?}", h.0)),
                Err(e) => log(&format!(
                    "selfhook: CreateWindowExW 失败 {e} —— 继续装钩子（可能不生效）"
                )),
            }

            let n = hook_own_threads();
            log(&format!("selfhook: hook thread ready hooks={n}"));
            // 必须在进消息循环**之前**发布：循环是阻塞的，函数永不返回。
            SELFHOOK_READY.store(n as i32, std::sync::atomic::Ordering::Release);

            // 保持线程存活：钩子链与安装线程的消息循环共生。
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            n
        }
    }

    // ------------------------------------------------------------ 共享内存

    struct ShmPtr(*mut u8);
    unsafe impl Send for ShmPtr {}
    unsafe impl Sync for ShmPtr {}
    static SHM: std::sync::OnceLock<Option<ShmPtr>> = std::sync::OnceLock::new();

    fn shm() -> Option<*mut u8> {
        let stored = SHM.get_or_init(|| unsafe {
            let mapping = CreateFileMappingW(
                HANDLE::default(),
                None,
                PAGE_READWRITE,
                0,
                (SLOT_COUNT * SLOT_SIZE) as u32,
                SHM_NAME,
            )
            .ok()?;
            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                return None;
            }
            Some(ShmPtr(view.Value as *mut u8))
        });
        stored.as_ref().map(|p| p.0)
    }

    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// 写控制块（clipx 侧请求测量时用）。
    pub fn write_request(seq: u32) {
        let Some(base) = shm() else { return };
        let tick = unsafe { GetTickCount64() };
        unsafe {
            let p = base as *mut u32;
            core::ptr::write_volatile(p.add(0), MAGIC_CTRL);
            core::ptr::write_volatile(p.add(1), PROTOCOL_VERSION);
            core::ptr::write_volatile(p.add(2), seq);
            core::ptr::write_volatile(p.add(3), tick as u32);
            core::ptr::write_volatile(p.add(4), (tick >> 32) as u32);
        }
    }

    fn publish_slot(rect: RECT, source: u32) {
        let Some(base) = shm() else { return };
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let slot = 1 + (seq as usize % (SLOT_COUNT - 1));
        let off = slot * SLOT_SIZE;
        let tick = unsafe { GetTickCount64() };
        let words: [u32; 16] = [
            MAGIC_SLOT,
            std::process::id(),
            (tick & 0xFFFF_FFFF) as u32,
            (tick >> 32) as u32,
            rect.left as u32,
            rect.top as u32,
            rect.right as u32,
            rect.bottom as u32,
            1, // dpi_aware 占位：真实 DPI 判定由 clipx 侧几何比对完成
            seq,
            source,
            0,
            0,
            0,
            0,
            0,
        ];
        unsafe {
            let p = base.add(off) as *mut u32;
            for (i, w) in words.iter().enumerate() {
                core::ptr::write_volatile(p.add(i), *w);
            }
        }
    }

    fn read_u32(base: *mut u8, index: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base as *const u32).add(index)) }
    }

    /// 信标常驻线程：轮询 clipx 的测量请求（request_seq 变化），
    /// 变化则测一次并写 ack_seq。
    ///
    /// 状态放在全局而不是捕获 `beacon` 引用：
    /// `Beacon_Impl` 不是 `'static`，且 `ITfThreadMgr` 不是 `Send`，
    /// 直接 move 进 `thread::spawn` 编译不过。
    struct GlobalState {
        mgr: Option<ITfThreadMgr>,
        client_id: u32,
    }
    // COM 指针跨线程使用：TSF 的 thread mgr 在本进程内线程安全引用，
    // 我们只在常驻线程里做只读测量。
    unsafe impl Send for GlobalState {}
    unsafe impl Sync for GlobalState {}

    static GLOBAL: std::sync::OnceLock<std::sync::Mutex<GlobalState>> =
        std::sync::OnceLock::new();

    fn global() -> &'static std::sync::Mutex<GlobalState> {
        GLOBAL.get_or_init(|| {
            std::sync::Mutex::new(GlobalState {
                mgr: None,
                client_id: 0,
            })
        })
    }

    fn ensure_worker() {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            std::thread::spawn(|| {
                let mut last_req = 0u32;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    let Some(base) = shm() else { continue };
                    let magic = read_u32(base, 0);
                    if magic != MAGIC_CTRL {
                        continue;
                    }
                    let req = read_u32(base, 2);
                    if req == last_req {
                        continue;
                    }
                    last_req = req;
                    log(&format!("request_seq={req} -> measuring"));
                    // 用全局 thread mgr / client id 主动测量。
                    let (tim, tid) = {
                        let guard = global().lock().unwrap();
                        (guard.mgr.clone(), guard.client_id)
                    };
                    if let Some(tim) = tim {
                        measure_caret(&tim, tid);
                    }
                    // ack
                    unsafe {
                        core::ptr::write_volatile((base as *mut u32).add(5), req);
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------ 导出

    #[no_mangle]
    extern "system" fn DllGetClassObject(
        rclsid: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> HRESULT {
        unsafe {
            if *rclsid != CLSID_BEACON {
                return CLASS_E_CLASSNOTAVAILABLE;
            }
            let factory: IClassFactory = Factory.into();
            factory.query(riid, ppv)
        }
    }

    #[no_mangle]
    extern "system" fn DllCanUnloadNow() -> HRESULT {
        S_FALSE // 常驻：sink 要活过 Deactivation
    }

    #[no_mangle]
    extern "system" fn DllRegisterServer() -> HRESULT {
        S_OK // 注册由安装脚本完成（HKLM + 机器级证书信任）
    }

    #[no_mangle]
    extern "system" fn DllUnregisterServer() -> HRESULT {
        S_OK
    }
}

// ------------------------------------------------------------ 非 Windows 存根

#[cfg(not(windows))]
mod stub {
    /// 非 Windows 平台不构建 TIP 实体，仅保留同名导出以免链接期报错。
    #[no_mangle]
    pub extern "system" fn DllGetClassObject() -> i32 {
        0
    }
    #[no_mangle]
    pub extern "system" fn DllCanUnloadNow() -> i32 {
        1
    }
    #[no_mangle]
    pub extern "system" fn DllRegisterServer() -> i32 {
        0
    }
    #[no_mangle]
    pub extern "system" fn DllUnregisterServer() -> i32 {
        0
    }
    #[no_mangle]
    pub extern "system" fn ClipxCaretHookProc() -> isize {
        0
    }
    #[no_mangle]
    pub extern "system" fn ClipxBeaconStart() -> i32 {
        0
    }
}

#[cfg(windows)]
pub use win::*;
