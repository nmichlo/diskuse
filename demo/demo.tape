# The README demo: `vhs demo/demo.tape`, after `demo/home.sh` and a
# release build. Browses a made-up home folder, so it shows no real paths.

Output demo/demo.gif
Require diskuse

Set Shell "bash"
Set FontSize 14
Set Width 1500
Set Height 400
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

Env HOME "/tmp/diskuse-demo/me"
Env DISKUSE_CACHE_DIR "/tmp/diskuse-demo/cache"
Env PS1 "$ "

Hide
Type "rm -rf /tmp/diskuse-demo/cache && cd ~ && clear"
Enter
Show

Type "diskuse ~"
Sleep 400ms
Enter
Sleep 2s

# into projects/, then webapp/, onto its node_modules
Down
Sleep 700ms
Right
Sleep 900ms
Down
Sleep 700ms
Right
Sleep 2.5s

# pick it, then list the picks
Space
Sleep 800ms
Type "p"
Sleep 2s
Type "p"
Sleep 600ms

# back home, onto Downloads/
Left
Sleep 600ms
Left
Sleep 800ms
Down
Sleep 2.5s
Type "q"
Sleep 500ms
