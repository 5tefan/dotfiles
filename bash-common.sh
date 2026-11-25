command -v starship >/dev/null && eval "$(starship init bash)"

alias ..="cd .."
alias ...="cd ../.."
alias ....="cd ../../.."
alias .....="cd ../../../.."

alias ls='ls --color=auto'
alias grep='grep --color=auto'

# Eternal bash history.
# ---------------------
# Undocumented feature which sets the size to "unlimited".
# http://stackoverflow.com/questions/9457233/unlimited-bash-history
export HISTFILESIZE=
export HISTSIZE=
export HISTTIMEFORMAT="[%F %T] "
# Change the file location because certain bash sessions truncate .bash_history file upon close.
# http://superuser.com/questions/575479/bash-history-truncated-to-500-lines-on-each-login
export HISTFILE=~/.bash_eternal_history
# Force prompt to write history after every command.
# http://superuser.com/questions/20900/bash-history-loss
PROMPT_COMMAND="history -a; $PROMPT_COMMAND"

export EDITOR="vim"
export HOST="localhost"
export WWW_HOME="https://duckduckgo.com/lite"

source $HOME/.z.sh

[ -d ~/.cargo/bin ] && export PATH=$PATH:~/.cargo/bin
