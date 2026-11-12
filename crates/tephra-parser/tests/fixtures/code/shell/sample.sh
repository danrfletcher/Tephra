#!/bin/bash
# line comment [[Line]]
# doc comment [[Doc]]
if [[ -f "$f" ]]; then echo "[[Decoy]]"; fi
cat <<EOF2
[[DecoyHeredoc]]
# [[DecoyHeredocHash]]
EOF2
