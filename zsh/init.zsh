
zmodload zsh/datetime
_zhis_cmd=""
_zhis_dir=""
_zhis_start=0

_zhis_preexec() {
	_zhis_cmd="$1"
	_zhis_dir="$PWD"
	_zhis_start=$EPOCHREALTIME
}

# Returns $ret so later precmd hooks (e.g. the prompt) still see the real
# exit status.
_zhis_precmd() {
	local ret=$?
	if [[ -n "$_zhis_cmd" ]]; then
		local cmd="$_zhis_cmd" dir="$_zhis_dir" start="$_zhis_start"
		_zhis_cmd=""
		if [[ "$cmd" != \ * ]]; then
			local first="${cmd%%$'\n'*}"
			first="${first%% *}"
			if (( ! ${+HIST_EXCLUDE} )) || [[ ${HIST_EXCLUDE[(ie)$first]} -gt ${#HIST_EXCLUDE} ]]; then
				# Integer assignment truncates the float result.
				local elapsed=0
				local -i ms=0
				(( start > 0 )) && elapsed=$(( EPOCHREALTIME - start ))
				(( ms = elapsed * 1000 ))
				(( start > 0 && elapsed >= 0 && ms == 0 )) && ms=1
				(( ms < 0 )) && ms=0
				print -r -- "$cmd" | zhis add -dir "$dir" -exit $ret -ms $ms
			fi
		fi
	fi
	return $ret
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _zhis_preexec
# Prepend so we read $? before other precmd hooks (prompt, atuin) clobber it.
# add-zsh-hook would append; filter instead so re-sourcing cannot stack copies.
precmd_functions=(_zhis_precmd ${precmd_functions:#_zhis_precmd})

_fhistory_select() {
	local qpwd=${(q)PWD}
	local dprompt="Dir> " gprompt="Global> "
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
	# Branches on $FZF_PROMPT so reloads keep whatever mode ctrl-g selected.
	local reload="if [ \"\$FZF_PROMPT\" = \"$dprompt\" ]; then zhis list$lim -dir $qpwd; else zhis list$lim; fi"
	local toggle="if [ \"\$FZF_PROMPT\" = \"$dprompt\" ]; then echo \"change-prompt($gprompt)+reload(zhis list$lim)\"; else echo \"change-prompt($dprompt)+reload(zhis list$lim -dir $qpwd)\"; fi"
	# Preview visibility persists across sessions via a flag file.
	local pstate="${XDG_STATE_HOME:-$HOME/.local/state}/zhis/preview-hidden"
	mkdir -p "${pstate:h}"
	local qstate=${(q)pstate}
	local pwin="down,6,wrap"
	[[ -f "$pstate" ]] && pwin="down,6,wrap,hidden"
	local id
	# Clear the user's fzf defaults so zhis renders the same on every machine.
	id=$(zhis list "${limarg[@]}" |
		FZF_DEFAULT_OPTS= FZF_DEFAULT_OPTS_FILE= \
		fzf --ansi --prompt="$gprompt" --tiebreak=index \
			--tabstop=1 --delimiter="$_zhis_delim" --with-nth="$_zhis_with_nth" \
			--preview="zhis get -id $idf" --preview-window=$pwin \
			--header="ctrl-g: dir/global · ctrl-d: delete entry · ctrl-x: delete all · ctrl-/: preview" \
			--bind "tab:accept" \
			--bind "ctrl-/:toggle-preview+execute-silent(if [ -f $qstate ]; then rm -f $qstate; else touch $qstate; fi)" \
			--bind "ctrl-g:transform:$toggle" \
			--bind "ctrl-d:execute-silent(zhis delete -id $idf)+reload($reload)" \
			--bind "ctrl-x:execute-silent(zhis delete -id $idf -all)+reload($reload)" |
		cut -d"$_zhis_delim" -f"$_zhis_id_field")
	[[ -n "$id" ]] && zhis get -id "$id"
}

_fhistory_widget() {
	if [[ -n "$BUFFER" && ("$KEYS" == $'\e[A' || "$KEYS" == $'\eOA') ]]; then
		zle up-line-or-history
	elif [[ -n "$BUFFER" && ("$KEYS" == $'\e[B' || "$KEYS" == $'\eOB') ]]; then
		zle down-line-or-history
	else
		local selected
		selected=$(_fhistory_select)
		if [[ -n "$selected" ]]; then
			BUFFER="$selected"
			CURSOR=${#BUFFER}
		fi
		zle reset-prompt
	fi
}
zle -N _fhistory_widget
bindkey '^R' _fhistory_widget
