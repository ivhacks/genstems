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
genstems --dissociate FILE.stem.mp4
```

`--dissociate` copies the master, instrumental and vocal tracks of a genstems .stem.mp4 out to `<title> - <Track>.flac` next to it, no transcoding.
