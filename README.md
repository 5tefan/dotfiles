# DOTFILES




## Shells

I do not keep `~/.bashrc` or `~/.zshrc` in the dotfiles repo because they are generally machine
specific. However, common pieces of configuration that should be used across machines I will put
into a "common" dotfile that I can then source from the machine specific rc file.

### Bash


### Zsh

Starting in 2021, I'm starting to migrate from Bash to Zsh as primary interactive shell.

- Plugin manager: oh-my-zsh https://github.com/ohmyzsh/ohmyzsh 
- Theme: Powerlevel10k https://github.com/romkatv/powerlevel10k

Before using Powerlevel10k, note that need to install a font. See p10k readme.


Using `zsh-common.zsh`: add `[ -f ~/.zsh-common.zsh ] && source ~/.zsh-common.zsh` to `~/.zshrc`.

