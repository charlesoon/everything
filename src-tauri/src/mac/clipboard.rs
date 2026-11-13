use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::AppResult;

pub(crate) fn copy_files(paths: &[String]) -> AppResult<()> {
    autoreleasepool(|_| write_file_urls(&NSPasteboard::generalPasteboard(), paths))
}

fn write_file_urls(pasteboard: &NSPasteboard, paths: &[String]) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    // One NSURL per pasteboard item lets Finder paste every selected file.
    // No AppleScript interpolation or conversion to plain path text is needed.
    let urls: Vec<_> = paths
        .iter()
        .map(|path| NSURL::fileURLWithPath(&NSString::from_str(path)))
        .collect();
    let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls
        .iter()
        .map(|url| ProtocolObject::from_ref(&**url))
        .collect();
    pasteboard.clearContents();
    if pasteboard.writeObjects(&NSArray::from_slice(&objects)) {
        Ok(())
    } else {
        Err("Failed to copy files to clipboard.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::NSPasteboardTypeFileURL;

    #[test]
    fn copies_multiple_file_urls_with_special_characters() {
        autoreleasepool(|_| {
            // A private pasteboard leaves the user's clipboard untouched.
            let pasteboard = NSPasteboard::pasteboardWithUniqueName();
            let paths = vec![
                "/tmp/한글 file \"one\".txt".into(),
                "/tmp/two\n#%.txt".into(),
            ];
            write_file_urls(&pasteboard, &paths).unwrap();
            let items = pasteboard.pasteboardItems().unwrap();
            assert_eq!(items.len(), paths.len());
            for (item, path) in items.iter().zip(&paths) {
                let url = item
                    .stringForType(unsafe { NSPasteboardTypeFileURL })
                    .unwrap();
                assert_eq!(
                    url,
                    NSURL::fileURLWithPath(&NSString::from_str(path))
                        .absoluteString()
                        .unwrap()
                );
            }
            pasteboard.clearContents();
        });
    }
}
