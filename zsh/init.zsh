
zmodload zsh/datetime
_zhis_cmd=""
_zhis_dir=""
_zhis_start=0

# Decides recording here rather than in precmd: an excluded command must not
# reach `zhis begin` either, or a leading space would still expose it to every
# picker for as long as it runs.
_zhis_preexec() {
	_zhis_cmd=""
	_zhis_dir="$PWD"
	_zhis_start=$EPOCHREALTIME
	local cmd="$1"
	[[ "$cmd" == \ * ]] && return
	local first="${cmd%%$'\n'*}"
	first="${first%% *}"
	if (( ${+HIST_EXCLUDE} )) && [[ ${HIST_EXCLUDE[(ie)$first]} -le ${#HIST_EXCLUDE} ]]; then
		return
	fi
	_zhis_cmd="$cmd"
	print -r -- "$cmd" | zhis begin -pid $$ -dir "$_zhis_dir"
}

# Returns $ret so later precmd hooks (e.g. the prompt) still see the real
# exit status.
_zhis_precmd() {
	local ret=$?
	if [[ -n "$_zhis_cmd" ]]; then
		local cmd="$_zhis_cmd" dir="$_zhis_dir" start="$_zhis_start"
		_zhis_cmd=""
		# Integer assignment truncates the float result.
		local elapsed=0
		local -i ms=0 ts=0
		(( start > 0 )) && elapsed=$(( EPOCHREALTIME - start ))
		(( ms = elapsed * 1000 ))
		(( start > 0 && elapsed >= 0 && ms == 0 )) && ms=1
		(( ms < 0 )) && ms=0
		# -ts is when the command started, not when it returned: a command that
		# ran for hours belongs where the user typed it. 0 falls back to now.
		(( ts = start ))
		# -pid clears the in-flight record `zhis begin` wrote.
		print -r -- "$cmd" | zhis add -pid $$ -dir "$dir" -exit $ret -ms $ms -ts $ts
	fi
	return $ret
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _zhis_preexec
# Prepend so we read $? before other precmd hooks (prompt, atuin) clobber it.
# add-zsh-hook would append; filter instead so re-sourcing cannot stack copies.
precmd_functions=(_zhis_precmd ${precmd_functions:#_zhis_precmd})

# $1 seeds fzf's query, so ctrl-r on a half-typed line searches for it.
_fhistory_select() {
	local qpwd=${(q)PWD}
	# Row layout comes from `zhis init`; see FIELD_DELIM/ID_FIELD in render.rs.
	local idf="{$_zhis_id_field}"
	# Unset means no limit: never silently hide old history. Set it to cap how
	# far back the picker loads, which also caps what a ctrl-d reload re-reads.
	# Digits only — $lim is interpolated into the reload strings fzf runs.
	local -a limarg=()
	if [[ -n "$ZHIS_LIST_LIMIT" ]]; then
		if [[ "$ZHIS_LIST_LIMIT" == <-> ]]; then
			limarg=(-limit "$ZHIS_LIST_LIMIT")
		else
			print -u2 "zhis: ignoring non-numeric ZHIS_LIST_LIMIT: $ZHIS_LIST_LIMIT"
		fi
	fi
	# fzf needs the same flag as a string; derive it so the two cannot drift.
	local lim="${limarg:+ ${(j: :)limarg}}"
	# Mode lives in a file, not the prompt: the info line renders it and every
	# reload — the first one included — reads it back, so nothing else states
	# it. One file per picker, so two shells cannot collide.
	local mfile
	mfile=$(mktemp "${TMPDIR:-/tmp}/zhis-mode.XXXXXX") || return
	print -r -- dir > "$mfile"
	local qm=${(q)mfile}
	local mread="IFS= read -r m < $qm"
	local reload="$mread; if [ \"\$m\" = dir ]; then zhis list$lim -dir $qpwd; else zhis list$lim; fi"
	local flip="$mread; if [ \"\$m\" = dir ]; then echo global > $qm; else echo dir > $qm; fi"
	# $FZF_INFO is the match counter fzf would have drawn on its own.
	local info="$mread; printf '%s %s' \"\$FZF_INFO\" \"\$m\""
	# The off-switch, not the per-row decision, persists across sessions.
	local pstate="${XDG_STATE_HOME:-$HOME/.local/state}/zhis/preview-hidden"
	mkdir -p "${pstate:h}"
	local qstate=${(q)pstate}
	# Starts hidden: the first focus event decides whether this row needs it.
	local pwin="up,1,wrap,noinfo,hidden"
	# The preview shows only what the row cannot: a command wider than its
	# column (20 cols: pointer, duration, age, scrollbar) or a multiline one.
	local off="change-preview-window(up,1,wrap,noinfo,hidden)"
	# Height is that command's wrapped height: fzf's border costs 2 columns, a
	# wrapped line 2 more. bg-transform resizes mid-draw, so: sync.
	local wrapped='BEGIN { if (w < 24) w = 80; a = w - 2; b = w - 4; row = w - 20 } { l = length($0); n += (l <= a ? 1 : 1 + int((l - a + b - 1) / b)); if (NR == 1 && l > row) cut = 1 } END { if (NR < 2 && !cut) exit 1; print (n > 10 ? 10 : n) }'
	local fit="[ -f $qstate ] && { echo \"$off\"; exit; }; n=\$(zhis get -id $idf | awk -v w=\"\$FZF_COLUMNS\" '$wrapped') || { echo \"$off\"; exit; }; echo \"change-preview-window(up,\$n,wrap,noinfo)\""
	local id
	# Clear the user's fzf defaults so zhis renders the same on every machine.
	id=$(FZF_DEFAULT_OPTS= FZF_DEFAULT_OPTS_FILE= \
		fzf --ansi --tiebreak=index --query="$1" \
			--tabstop=1 --delimiter="$_zhis_delim" --with-nth="$_zhis_with_nth" \
			--info-command="$info" \
			--preview="zhis get -id $idf" --preview-window=$pwin \
			--bind "start:reload($reload)" \
			--bind "tab:accept" \
			--bind "ctrl-/:execute-silent(if [ -f $qstate ]; then rm -f $qstate; else touch $qstate; fi)+transform:$fit" \
			--bind "ctrl-g:execute-silent($flip)+reload($reload)" \
			--bind "focus:transform:$fit" \
			--bind "resize:transform:$fit" \
			--bind "ctrl-d:execute-silent(zhis delete -id $idf)+reload($reload)" \
			--bind "ctrl-x:execute-silent(zhis delete -id $idf -all)+reload($reload)" \
			< /dev/null |
		cut -d"$_zhis_delim" -f"$_zhis_id_field")
	rm -f "$mfile"
	[[ -n "$id" ]] && zhis get -id "$id"
}

_fhistory_widget() {
	if [[ -n "$BUFFER" && ("$KEYS" == $'\e[A' || "$KEYS" == $'\eOA') ]]; then
		zle up-line-or-history
	elif [[ -n "$BUFFER" && ("$KEYS" == $'\e[B' || "$KEYS" == $'\eOB') ]]; then
		zle down-line-or-history
	else
		local selected
		selected=$(_fhistory_select "$BUFFER")
		if [[ -n "$selected" ]]; then
			BUFFER="$selected"
			CURSOR=${#BUFFER}
		fi
		zle reset-prompt
	fi
}
zle -N _fhistory_widget
# Every keymap explicitly: a bare bindkey hits only the current one, which under
# `bindkey -v` leaves vicmd's ctrl-r on whatever plugin bound it last.
bindkey -M emacs '^R' _fhistory_widget
bindkey -M viins '^R' _fhistory_widget
bindkey -M vicmd '^R' _fhistory_widget
