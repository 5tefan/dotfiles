alias ..="cd .."
alias ...="cd ../.."
alias ....="cd ../../.."
alias .....="cd ../../../.."

export HISTFILE="$HOME/.zsh_history"
export HISTSIZE=9999999
export SAVEHIST=$HISTSIZE
setopt inc_append_history
setopt hist_ignore_all_dups
setopt hist_ignore_space

export WWW_HOME="https://duckduckgo.com/lite"
export EDITOR="vim"

alias ls='ls --color=auto'
alias grep='grep --color=auto'


