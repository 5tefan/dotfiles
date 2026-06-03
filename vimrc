call plug#begin()

    Plug 'tpope/vim-sensible'
    " git wrapper
    Plug 'tpope/vim-fugitive'

    Plug 'vimwiki/vimwiki', { 'branch': 'dev' }
    Plug 'scrooloose/nerdtree'
    Plug 'junegunn/fzf', { 'dir': '~/.fzf', 'do': './install --all' }
    Plug 'junegunn/fzf.vim'
    Plug 'lervag/vimtex'
    Plug 'leafgarland/typescript-vim'

    " Surround text objects with quotes, etc.
    Plug 'tpope/vim-surround'

    " Repeat surrounds, etc.
    Plug 'tpope/vim-repeat'

    " Vertical lines on code blocks:
    Plug 'Yggdroot/indentLine'

    " \ww to jump
    Plug 'easymotion/vim-easymotion'

    " \cc to comment out, \cu to uncomment
    Plug 'preservim/nerdcommenter'

    " Python aware text objects like [f]unction, [c]lass:
    Plug 'jeetsukumaran/vim-pythonsense'

call plug#end()

" Backspace over everything.
set backspace=indent,eol,start

set noincsearch
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

" keep 5 lines context on cursor... eg. on search / scroll
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
                \ '~/.vim/plugged/fzf.vim/bin/preview.sh {}'
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
                \ '~/.vim/plugged/fzf.vim/bin/preview.sh {}'
            \ ]}, <bang>0)


"" Vimwiki....
let wiki_1 = {}
let wiki_1.path = '~/owncloud/vimwiki'
let wiki_1.path_html = '~/owncloud/vimwiki_html/'
let wiki_1.template_path = '~/owncloud/vimwiki/templates/'
let wiki_1.template_default = 'default'
let wiki_1.template_ext = '.html'
let wiki_1.css_name = '~/owncloud/vimwiki/custom.css'

" For some reason, this conceal seems to be leaking around and
" is super annoying. I don't want magically appearing chars!
let g:vimwiki_conceal_onechar_markers = 0
" disables link conceal.
let g:vimwiki_conceallevel = 0

" indentLine looks like another culript.
let g:indentLine_fileTypeExclude = ['json', 'vimwiki']
let g:indentLine_setConceal = 0

let g:vimwiki_conceal_onechar_markers=0
let g:vimwiki_list = [wiki_1]

autocmd FileType * setlocal fo-=c fo-=r fo-=o

" wrap on lines longer than tw
set fo+=t

" vimtex default flavor, rather than default plain
let g:tex_flavor = 'latex'
" please don't conceal things!
let g:vimtex_syntax_conceal_disable = 1

" Helper function to handle PGN includes with unique IDs
function! VimwikiHandlePgnInclude(value)
    " Matches content between {{pgn: and }}
    let pgn = matchstr(a:value, '{{pgn:\zs.\{-}\ze}}')
    if filereadable(pgn)
        let lines = readfile(pgn)
        " Filter out lines starting with [Site "
        let pattern = '^[Site "'
        let filtered_lines = filter(lines, 'v:val !~# pattern')
        let pgn_raw = join(filtered_lines, "\r")
    else
        return ''
    endif

    if !exists('g:vimwiki_pgn_counter')
        let g:vimwiki_pgn_counter = 0
    endif
    let g:vimwiki_pgn_counter += 1
    let unique_id = 'chess_board_pgn_' . g:vimwiki_pgn_counter

    " Store escaped PGN in hidden div, script reads and uses it
    let out = '<div id="' . unique_id . '" class="cg-wrap"></div>'
    let out .= '<div id="' . unique_id . '_pgn" style="display:none">' . pgn_raw . '</div>'
    let out .= '<script type="module">import V from "/js/lichess-pgn-viewer.min.js"; var el = document.getElementById("' . unique_id . '"); var pgnEl = document.getElementById("' . unique_id . '_pgn"); var pgn = pgnEl.textContent.replace(/<br>/g, "\r"); var v = V(el, {pgn: pgn});</script>'
    return out
endfunction

" Helper function to handle FEN includes
function! VimwikiHandleFenInclude(value)
    let fen = matchstr(a:value, '{{fen:\zs.\{-}\ze}}')
    let wfen = matchstr(a:value, '{{wfen:\zs.\{-}\ze}}')
    let bfen = matchstr(a:value, '{{bfen:\zs.\{-}\ze}}')

    let orientation = "white"
    if strlen(wfen) > 3
        let fen = wfen
    elseif strlen(bfen) > 3
        let fen = bfen
        let orientation = "black"
    endif

    if strlen(fen) > 3
        if !exists('g:vimwiki_fen_counter')
            let g:vimwiki_fen_counter = 0
        endif
        let g:vimwiki_fen_counter += 1
        let unique_id = 'chess_board_fen_' . g:vimwiki_fen_counter

        let out = '<div class="fen"><div id="' . unique_id . '" class="cg-wrap"></div></div>'
        let out .= '<script>var cg = Chessground(document.getElementById("' . unique_id . '"), {"fen": "' . fen . '", "orientation": "' . orientation . '"})</script>'
        let out .= '<br/><br/>'
        let out .= '<pre>'.fen.'</pre>'
        return out
    end
    " Return the empty string when unable to process link
    return ''
endfunction

function! VimwikiWikiIncludeHandler(value)
    call writefile(['VimwikiWikiIncludeHandler called with: ' . string(a:value)], expand('~') . '/tmp/vimwiki_debug.log', 'a')
    if a:value =~ '{{pgn:'
        return VimwikiHandlePgnInclude(a:value)
    elseif a:value =~ '{{fen:' || a:value =~ '{{wfen:' || a:value =~ '{{bfen:'
        return VimwikiHandleFenInclude(a:value)
    endif
    return ''
endfunction

" copy previous commit message as a starting point, Git Log Copy
command Glc .-1read !git log --format=\%s -n1

" read date
command Rd .-1read !date -I
command Rdt .-1read !date

" quick fix shortcuts, write and next/previous
command! -nargs=0 Wcn :w <bar> :cn
command! -nargs=0 Wcp :w <bar> :cp
cnoreabbrev wcn Wcn
cnoreabbrev wcp Wcp

" repeat last macro
map , @@
