# DOTFILES




## Shells

I do not keep `~/.bashrc` or `~/.zshrc` in the dotfiles repo because they are generally machine
specific. However, common pieces of configuration that should be used across machines I will put
into a "common" dotfile that I can then source from the machine specific rc file.

### Bash

1. Add `[ -f ~/.bash-common.sh ] && source ~/.bash-common.sh` to `~/.bashrc`

### Zsh

Starting in 2021, I'm starting to migrate from Bash to Zsh as primary interactive shell.

- Plugin manager: oh-my-zsh https://github.com/ohmyzsh/ohmyzsh 
- Theme: Powerlevel10k https://github.com/romkatv/powerlevel10k

Before using Powerlevel10k, note that need to install a font. See p10k readme.

0. Add `[ -f ~/.zsh-common.zsh ] && source ~/.zsh-common.zsh` to `~/.zshrc`.
1. Install oh-my-zsh: `sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)"`
2. Install pl10k: `git clone --depth=1 https://github.com/romkatv/powerlevel10k.git ${ZSH_CUSTOM:-$HOME/.oh-my-zsh/custom}/themes/powerlevel10k`
3. Set `ZSH_THEME="powerlevel10k/powerlevel10k"` in `~/.zshrc`
4. Add ` [[ ! -f ~/.p10k.zsh ]] || source ~/.p10k.zsh` in `~/.zshrc`

