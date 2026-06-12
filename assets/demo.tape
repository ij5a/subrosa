# Regenerate the README demo GIF: scripts/demo-data.sh first, then `vhs assets/demo.tape`.
# Records against the synthetic demo archive only — never live data.
Output assets/demo.gif

Set FontSize 18
Set Width 1200
Set Height 700
Set Padding 24
Set Theme "Rose Pine"
Set TypingSpeed 30ms
Set WindowBar Colorful
Set BorderRadius 8
Set Margin 16
Set MarginFill "#0d1117"

Hide
Type "export SUBROSA_DIR=/tmp/subrosa-demo/data SUBROSA_PROJECTS_DIR=/tmp/subrosa-demo/projects && cd /tmp && rm -f /tmp/subrosa-demo/data/recall-seen.log && clear"
Enter
Show

Type@15ms "# subrosa — every Claude Code session, archived locally and searchable"
Enter
Sleep 2200ms

Type@15ms "# you fixed this three weeks ago. Claude forgot. subrosa didn't:"
Enter
Sleep 1200ms
Type "subrosa search aurora failover"
Sleep 500ms
Enter
Sleep 6s

Type@15ms "# every prompt is checked — matching sessions land in Claude's context:"
Enter
Sleep 1200ms
Type `echo '{"prompt":"why was the aurora failover slow?","cwd":"/work/acme/api","session_id":"demo"}' | subrosa hook user-prompt-submit`
Sleep 500ms
Enter
Sleep 5500ms

Type@15ms "# the whole archive — one local file, zero cloud, zero tokens:"
Enter
Sleep 1200ms
Type "subrosa stats"
Sleep 500ms
Enter
Sleep 6s
