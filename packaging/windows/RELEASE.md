## Windows sound cards on the Dante network

The installers add Inferno's virtual sound cards to Windows: sound devices whose playback channels are transmitted to the Dante network and whose recording channels are received from it. Download the one for your architecture (x64 or ARM64), run it and approve the administrator prompt. Restart Windows if prompted.

- The installer includes the signed [usbip-win2](https://github.com/vadimgrn/usbip-win2) driver (0.9.8.1) and installs it only if it's missing. Uninstalling leaves it installed, because other applications such as Win AirPlay can use it at the same time.
- The service starts at sign-in and adds a tray icon. Click it to open the settings page (`http://127.0.0.1:47800/`). There you can set up the sound cards and their channels, and the device's name on the network.
- Route the channels with Dante Controller or netaudio, as for any Dante device. The device follows the network's PTP clock: the cards appear once it has one.
- Most Windows applications (shared mode) open at most 32 channels of a sound card. Larger cards work with applications that open them in exclusive mode.

Logs are at `%LOCALAPPDATA%\Inferno\soundcard.log`.
