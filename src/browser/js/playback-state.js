// 來源：$REF/dist/browser/StreamPlaybackOptimizer.js（applyStreamPlaybackOptimizations 內 video.evaluate 的函式）
// 由 S1.2 逐字複製；修改前先寫 doc/decisions.md。
(element, shouldMute) => {
                const video = element;
                if (shouldMute) {
                    video.muted = true;
                    video.volume = 0;
                    video.disablePictureInPicture = true;
                }
                return {
                    muted: shouldMute && video.muted && video.volume === 0,
                    ...(typeof video.videoHeight === 'number' && video.videoHeight > 0
                        ? { videoHeight: video.videoHeight }
                        : {}),
                };
            }
