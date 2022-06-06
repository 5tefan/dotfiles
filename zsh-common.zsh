alias ..="cd .."
alias ...="cd ../.."
alias ....="cd ../../.."
alias .....="cd ../../../.."

export HISTFILE="$HOME/.zsh_history"
export HISTSIZE=9999999
export SAVEHIST=$HISTSIZE

# manually import history with `fc -RI`
unsetopt SHARE_HISTORY
setopt INC_APPEND_HISTORY
setopt HIST_FIND_NO_DUPS
setopt HIST_IGNORE_SPACE
setopt HIST_REDUCE_BLANKS

export WWW_HOME="https://duckduckgo.com/lite"
export EDITOR="vim"

alias ls='ls --color=auto'
alias grep='grep --color=auto'
alias rg='rg --color=always'

source $HOME/.z.sh

