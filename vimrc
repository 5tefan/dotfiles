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
Plugin 'leafgarland/typescript-vim'

" Surround text objects with quotes, etc.
Plugin 'tpope/vim-surround'

" Repeat surrounds, etc.
Plugin 'tpope/vim-repeat'

" Vertical lines on code blocks:
Plugin 'Yggdroot/indentLine'

" \ww to jump
Plugin 'easymotion/vim-easymotion'

" \cc to comment out, \cu to uncomment
Plugin 'preservim/nerdcommenter'

" Python aware text objects like [f]unction, [c]lass:
Plugin 'jeetsukumaran/vim-pythonsense'

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

" Backspace over everything.
set backspace=indent,eol,start  

set nu
set ruler
set list

" Always show status line
set laststatus=2

" Do not convert spaces into tabs.
set softtabstop=0 expandtab

" Tab char appears 4 spaces wide
set tabstop=4

" Pressing tab inserts 4 spaces.
set shiftwidth=4 smarttab

" Indent by 4 spaces when using >>, <<, == etc.
set shiftwidth=4

" Indent by 4 spaces when pressing <TAB>
set softtabstop=4

" Hilight the current line (and the line numbers)
set cursorline

" Show matching parenthesis
set showmatch

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

" When using :edit autocomplete, deprioritize files named with these extensions:
set suffixes=.aux,.bak,.dvi,.idx,.ps,.swp,.swo,.tar

" Automatically save modifications to files on :next
set autowrite

" In regular use, keep a per file undo history
if !isdirectory($HOME."/.vim/undo")
     call mkdir($HOME."/.vim/undo", "", 0700)
endif
set undodir=~/.vim/undo
set undofile

" ...... Plugin settings .......

" fzf respect .rgignore files
let $FZF_DEFAULT_COMMAND = 'rg --files'

" short version :F of normal :Files cmd
command! -bang -nargs=? -complete=dir F call fzf#vim#files(<q-args>, 
            \ {'options': [
                \ '--info=inline', 
                \ '--preview',
                \ '~/.vim/bundle/fzf.vim/bin/preview.sh {}'
            \ ]}, <bang>0)

" Rg with preview
command! -bang -nargs=* Rg call fzf#vim#grep(
            \ "rg --column --line-number --no-heading --color=always --smart-case "
            \ .shellescape(<q-args>),
            \ 1,
            \ {'options': [
                \ '--delimiter=:',
                \ '--nth=4..',
                \ '--preview',
                \ '~/.vim/bundle/fzf.vim/bin/preview.sh {}'
            \ ]}, <bang>0)


"" Vimwiki....
let wiki_1 = {}
let wiki_1.path = '~/ownCloud2/vimwiki'
let wiki_1.path_html = '~/ownCloud2/vimwiki_html/'
let wiki_1.template_path = '~/ownCloud2/vimwiki/templates/'
let wiki_1.template_default = 'default'
let wiki_1.template_ext = '.html'
let wiki_1.css_name = '~/ownCloud2/vimwiki/custom.css'

let g:vimwiki_list = [wiki_1]
" For some reason, this conceal seems to be leaking around and
" is super annoying. I don't want magically appearing chars!
let g:vimwiki_conceal_onechar_markers = 0
" disables link conceal.
let g:vimwiki_conceallevel = 0

" indentLine looks like another culript.
let g:indentLine_fileTypeExclude = ['json', 'vimwiki']
let g:indentLine_setConceal = 0

autocmd FileType * setlocal formatoptions-=c formatoptions-=r formatoptions-=o

" vimtex default flavor, rather than default plain
let g:tex_flavor = 'latex'
" please don't conceal things!
let g:vimtex_syntax_conceal_disable = 1

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

" copy previous commit message as a starting point, Git Log Copy
command Glc .-1read !git log --format=\%s -n1

" read date
command Rd .-1read !date -I

" quick fix shortcuts, write and next/previous
command! -nargs=0 Wcn :w <bar> :cn
command! -nargs=0 Wcp :w <bar> :cp
cnoreabbrev wcn Wcn
cnoreabbrev wcp Wcp

" repeat last macro
map , @@
