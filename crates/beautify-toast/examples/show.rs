//! Throwaway visual check: a two-line detail toast, the case that used to
//! clip. Run it, screenshot the top-left corner, then delete this file.
use std::time::Duration;

fn main() {
    let toast = beautify_toast::Toast::new();
    toast.show(
        "已切换音频设备",
        "扬声器：扬声器 (ToDesk Virtual Audio)\n麦克风：麦克风 (ToDesk Virtual Audio)",
    );
    std::thread::sleep(Duration::from_millis(6000));
}
