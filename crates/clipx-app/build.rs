fn main() {
    // 单一编译入口：popup.slint 再导出 quickfind.slint 的窗口。
    // slint_build::compile 每次调用都会覆盖 SLINT_INCLUDE_GENERATED，
    // 多次编译只有最后一个文件会被 include_modules! 引入。
    slint_build::compile("ui/popup.slint").expect("failed to compile slint ui");
}
