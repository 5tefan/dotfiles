set nocompatible              " be iMproved, required
filetype off                  " required

" set the runtime path to include Vundle and initialize
set rtp+=~/.vim/bundle/Vundle.vim
call vundle#begin()
" let Vundle manage Vundle, required
Plugin 'VundleVim/Vundle.vim'

" git wrapper
Plugin 'tpope/vim-fugitive'

Plugin 'vimwiki/vimwiki'
Plugin 'scrooloose/nerdtree'
Plugin 'junegunn/fzf', { 'dir': '~/.fzf', 'do': './install --all' }
Plugin 'junegunn/fzf.vim'
Plugin 'lervag/vimtex'

" Surround text objects with quotes, etc.
Plugin 'tpope/vim-surround'

" Repeat surrounds, etc.
Plugin 'tpope/vim-repeat'

" Vertical lines on code blocks:
Plugin 'Yggdroot/indentLine'

" Python aware text objects like [f]unction, [c]lass: Plugin 'jeetsukumaran/vim-pythonsense'

" \ww to jump
Plugin 'easymotion/vim-easymotion'

" \cc to comment out, \cu to uncomment
Plugin 'preservim/nerdcommenter'

" All of your Plugins must be added before the following line
call vundle#end()            " required
filetype plugin indent on    " required
" To ignore plugin indent changes, instead use:
"filetype plugin on
"
" Brief help
" :PluginList       - lists configured plugins
" :PluginInstall    - installs plugins; append `!` to update or just :PluginUpdate
" :PluginSearch foo - searches for foo; append `!` to refresh local cache
" :PluginClean      - confirms removal of unused plugins; append `!` to auto-approve removal
"
" see :h vundle for more details or wiki for FAQ
" Put your non-Plugin stuff after this line
" Turn filetype plugin back on...
" See :help ftplugin
filetype plugin on


syntax on
set backspace=indent,eol,start  " more powerful backspacing
set nu
set laststatus=2
set tabstop=4
set softtabstop=0
set expandtab
set shiftwidth=4
set smarttab
set ruler
set list

set tw=100
set history=10000 " this is the max value

" If you search for something containing uppercase characters, it will do a case sensitive search; 
" if you search for something purely lowercase, it will do a case insensitive search. 
" NOTE: ignorecase affects substitutions as well as searches.
set ignorecase
set smartcase

" keep 5 lines context on curosr... eg. when searching
set scrolloff=5

" highlight non-ascii characters: https://stackoverflow.com/a/16988346
syntax match nonascii "[^\x00-\x7F]"
highlight nonascii guibg=Red ctermbg=2

" when wrapping lines, move by displayed lines.
noremap <silent> k gk
noremap <silent> j gj


" ...... Plugin settings .......

" fzf respect .rgignore files
let $FZF_DEFAULT_COMMAND = 'rg --files'
" short version :F of normal :Files cmd
command! -bang -nargs=? -complete=dir F call fzf#vim#files(<q-args>, <bang>0)

"" Vimwiki....
let wiki_1 = {}
let wiki_1.path = '~/ownCloud/vimwiki'
let wiki_1.path_html = '~/ownCloud/vimwiki_html/'
let wiki_1.template_path = '~/ownCloud/vimwiki/templates/'
let wiki_1.template_default = 'default'
let wiki_1.template_ext = '.html'
let wiki_1.css_name = '~/ownCloud/vimwiki/custom.css'

let wiki_2 = {}
let wiki_2.path = '~/ownCloud/vimwiki/work'
let wiki_2.path_html = '~/ownCloud/vimwiki_html/work/'

let g:vimwiki_list = [wiki_1, wiki_2]
" For some reason, this conceal seems to be leaking around and
" is super annoying. I don't want magically appearing chars!
let g:vimwiki_conceal_onechar_markers = 0
" disables link conceal.
let g:vimwiki_conceallevel = 0

" indentLine looks like another culript.
let g:indentLine_fileTypeExclude = ['json', 'vimwiki']

autocmd FileType * setlocal formatoptions-=c formatoptions-=r formatoptions-=o

" vimtex default flavor, rather than default plain
let g:tex_flavor = 'latex'
" please don't conceal things!
let g:vimtex_syntax_conceal_default = 0

" Vimwiki include pgn files with {{pgn:path/to.pgn}}
function! VimwikiWikiIncludeHandler(value)
    let pgn = matchstr(a:value, '{{pgn:\zs.\{-}\ze}}')
    if filereadable(pgn)
        return '<pre class="pgn">'.join(readfile(pgn), "\r").'</pre>'
    end
    " Return the empty string when unable to process link
    return ''
endfunction

" Vimwiki include fen board positions with {{fen:...}}
" Need to configure chessground assets in (likely) a template.
" before this will work for you.
function! VimwikiWikiIncludeHandler(value)
    let fen = matchstr(a:value, '{{fen:\zs.\{-}\ze}}')
    if strlen(fen) > 3
        let out = '<div class="blue merida"><div id="dirty" class="cg-wrap"></div></div>'
        let out .= '<script>var cg = Chessground(document.getElementById("dirty"), {"fen": "'.fen.'"})</script>'
        let out .= '<br/><br/>'
        let out .= '<pre class="fen">'.fen.'</pre>'
        return out
    end
    " Return the empty string when unable to process link
    return ''
endfunction


command! -bang -nargs=* Rg call fzf#vim#grep("rg --column --line-number --no-heading --color=always --smart-case "
            \.shellescape(<q-args>), 1, {'options': '--delimiter : --nth 4..'}, <bang>0) 

