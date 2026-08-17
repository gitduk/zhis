# zhis plugin entry point for zsh plugin managers (zinit, zsh-snap, ...).
# The zhis binary must be installed separately; see the README.
(( $+commands[zhis] )) && eval "$(zhis init)"
