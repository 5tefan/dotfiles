
let $FZF_DEFAULT_COMMAND = 'rg --files'

" short version :F of normal :Files cmd
command! -bang -nargs=? -complete=dir F call fzf#vim#files(<q-args>, 
            \ {'options': [
                \ '--info=inline', 
                \ '--preview',
                \ '~/.local/share/nvim/plugged/fzf.vim/bin/preview.sh {}'
            \ ]}, <bang>0)

command! -bang -nargs=* Rg call fzf#vim#grep("rg --column --line-number --no-heading --color=always --smart-case "
            \.shellescape(<q-args>), 1, {'options': '--delimiter : --nth 4..'}, <bang>0) 
