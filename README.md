# genstems

turns a song (.flac, .mp3) into an NI stem file (.stem.mp4) with master, instrumental and vocal tracks. see AGENTS.md for the details.

## deps

genstems shells out to these, and checks for all of them before doing any work:

| tool | debian | fedora |
| --- | --- | --- |
| ffmpeg, ffprobe | `apt install ffmpeg` | `dnf install ffmpeg` |
| metaflac | `apt install flac` | `dnf install flac` |
| MP4Box | `apt install gpac` (older/minimal releases may need a [source build](https://github.com/gpac/gpac)) | `dnf install gpac` |
| curl (`--split` only) | `apt install curl` | `dnf install curl` |

## usage

```sh
genstems --master FILE --vocal FILE --instrumental FILE [--output FILE]
genstems --split FILE --token TOKEN [--output FILE]
genstems --colors FILE.stem.mp4
genstems --swap-tracks FILE.stem.mp4
genstems --dissociate FILE.stem.mp4 [--output DIR]
```

`--dissociate` breaks a .stem.mp4 back into one file per track (master + stems), stream-copied with no transcoding, so each keeps its codec (flac → .flac, aac/alac → .m4a, mp3 → .mp3). output goes to `<title> stems/` next to the input unless `--output DIR`. silent filler tracks (named `-`) are skipped.
