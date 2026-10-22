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
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, HC_ACTION};

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
                measure_context(&ctx, self.client_id.get());
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
                publish_slot(rect, 1);
                self.rect.set(Some(rect));
            }
            Ok(())
        }
    }

    fn measure_context(ctx: &ITfContext, tid: u32) {
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
    }

    fn measure_caret(tim: &ITfThreadMgr, tid: u32) {
        unsafe {
            let Ok(dm) = tim.GetFocus() else {
                log("measure: thread mgr has no focus");
                return;
            };
            let Ok(ctx) = dm.GetBase() else {
                log("measure: no base context");
                return;
            };
            measure_context(&ctx, tid);
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
        if code == HC_ACTION as i32 {
            if let Some(base) = shm() {
                let req = read_u32(base, 2);
                let ack = read_u32(base, 5);
                if req != 0 && req != ack {
                    log(&format!("inject hook: request_seq={req} -> measuring"));
                    measure_current_thread();
                    core::ptr::write_volatile((base as *mut u32).add(5), req);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
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
}

#[cfg(windows)]
pub use win::*;
