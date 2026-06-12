# Regenerate the README demo GIF: scripts/demo-data.sh first, then `vhs assets/demo.tape`.
# Records against the synthetic demo archive only — never live data.
Output assets/demo.gif

Set FontSize 16
Set Width 1200
Set Height 660
Set Padding 24
# jp's glitch theme (ij5a/konsol-sh-themes, warp/glitch.yaml) inlined for vhs.
Set Theme { "name": "glitch", "background": "#0D0208", "foreground": "#00FF41", "cursor": "#00FF41", "selection": "#008F11", "black": "#0D0208", "red": "#8B1A1A", "green": "#008F11", "yellow": "#B8860B", "blue": "#1E4D5B", "magenta": "#6B2C8A", "cyan": "#008B8B", "white": "#00B82C", "brightBlack": "#1A1008", "brightRed": "#C41E3A", "brightGreen": "#00FF41", "brightYellow": "#DAA520", "brightBlue": "#4682B4", "brightMagenta": "#A076C4", "brightCyan": "#00CED1", "brightWhite": "#A0FFB0" }
Set TypingSpeed 30ms

Hide
Type "export SUBROSA_DIR=/tmp/subrosa-demo/data SUBROSA_PROJECTS_DIR=/tmp/subrosa-demo/projects && rm -f /tmp/subrosa-demo/data/recall-seen.log && clear"
Enter
Show

Type "# three weeks ago you fixed a slow Aurora failover. Claude forgot. subrosa didn't:"
Enter
Sleep 800ms
Type "subrosa search aurora failover"
Enter
Sleep 3500ms

Type "# every prompt you type is checked against the archive — hits land in Claude's context:"
Enter
Sleep 800ms
Type `echo '{"prompt":"why was the aurora failover slow?","cwd":"/work/acme/api","session_id":"demo"}' | subrosa hook user-prompt-submit`
Enter
Sleep 3500ms

# --no-color: ttyd renders SGR-faint nearly invisible on glitch's near-black.
Type "# the whole archive, one local file:"
Enter
Sleep 600ms
Type "subrosa stats --no-color"
Enter
Sleep 5000ms
