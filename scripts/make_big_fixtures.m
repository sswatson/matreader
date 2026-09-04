function make_big_fixtures(outdir)
% Larger fixtures for exercising lazy loading. Kept out of the repo.
rng(1);
tallBig = randn(5e6, 4);            % 160 MB, tall: tests 2-D windowed reads
wideBig = randn(4, 5e6);            % 160 MB, wide
cube = randn(300, 300, 300);        % 216 MB, 3-D
small = magic(5);
manyCells = num2cell(1:100000);     % 100k cells
bigStructArr = struct('v', num2cell(1:20000), 'name', repmat({'n'}, 1, 20000));
save(fullfile(outdir, 'big_v7.mat'), 'tallBig', 'wideBig', 'cube', 'small', 'manyCells', 'bigStructArr', '-v7');
save(fullfile(outdir, 'big_v73.mat'), 'tallBig', 'wideBig', 'cube', 'small', 'manyCells', 'bigStructArr', '-v7.3');
fprintf('wrote big fixtures to %s\n', outdir);
end
