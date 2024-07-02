# Online Sequence Automatic Renderer

This is a tool to render sequences automatically using a web interface or API. A render
may be initiated either via the index page (by entering a URL or sequence ID and clicking
the Go button), or sending a POST request to `http://127.0.0.1:3154/render/<id>/start`.
Additionally, the progress of a render may be tracked using SSE by connecting to `http://127.0.0.1:3154/render/<id>/poll`.
The resulting audio files are available under `/renders/<id>.<ogg|mp3|wav>`.

To use this tool, you must have both [ffmpeg](https://ffmpeg.org/) and [chromedriver](https://developer.chrome.com/docs/chromedriver/downloads)
installed, as well as a working Chromium or Chrome installation. Run the program using Cargo
(available from [rustup](https://rustup.rs/)) by executing `cargo run` within the repository
directory. The web interface is accessible via `http://127.0.0.1:3154` by default.

All code in this repository is licensed under the MIT License.

## Usage
```
Usage: render-service [OPTIONS]

Options:
  -o, --origin <ORIGIN>
          Address of the origin server to render sequences on [default: https://onlinesequencer.net]
  -s, --sequence-origin <SEQUENCE_ORIGIN>
          Address of the server to retrieve sequences from (without the trailing slash) [default: https://onlinesequencer.net]
  -d, --driver-executable <DRIVER_EXECUTABLE>
          Path to the chromedriver executable [default: chromedriver]
  -f, --ffmpeg-executable <FFMPEG_EXECUTABLE>
          Path to the ffmpeg executable [default: ffmpeg]
  -r, --renders <RENDERS>
          Maximum parallel renders [default: 10]
  -h, --help
          Print help
  -V, --version
          Print version
```
